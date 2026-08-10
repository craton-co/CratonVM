# `HIB-CV-32`-labeled corrupt-Value-cell guard — confirmed across 8 apps, confirmed absent on `dev`, confirmed fixed by merging `dev` into `main`

**Status:** RESOLVED as of the 2026-08-10 `dev`→`main` merge on the Azure
build host (`/data/cratonvm`, `70bf05ed3` → `681b5c1f1`), for the specific
symptom described here. The underlying `dev` commit(s) that actually fixed
it were not bisected — this doc establishes *that* the merge fixes it and
roughly *how much* of the total non-pass rate it explains (not much), not
*which commit* did it.

## The symptom, as originally seen

```
ERROR cratonvm::gc::guard: gen_heap::read_slot: corrupt Value cell
(out-of-range discriminant) — returning null instead of a UB-on-match
Value. Heap reference-integrity defect (see HIB-CV-32).
```

The `(see HIB-CV-32)` citation is misleading on its own — it is a generic
guard message baked into `gen_heap::read_slot`, not proof that whatever
fired it is the *original* HIB-CV-32 bug
(`../../../known-issues/hibernate` — the original was fixed 2026-07-16, needed 24 concurrent
threads + heavy Stream API use to reproduce, and is unrelated to what's
described here beyond sharing a log string).

## Cross-app corroboration (2026-08-10, pre-merge, commit `70bf05ed3`)

Independently hit by suite-runner sessions for **8 different apps** built
against the same pre-merge binary, at hit rates correlating with (but not
strictly gated by) classpath size:

| app | classpath size | hit rate | fatal? |
|---|---|---:|---|
| h2database | ~2.2KB, ~30 entries | ~9% (2/23 sampled) | no — guarded recovery |
| tomcat (JUnit4) | medium | 70% (14/20) | mixed |
| netty | medium-large | 75% (15/20) | crash |
| quarkus | medium | 100% (20/20) | crash |
| hibernate-orm | 243 entries | 100% | crash |
| hibernate-reactive | large | 100% (7/7) | crash |
| spring-boot | large | 100% (15/15, both JIT modes) | crash |
| spring-framework | large | 100% (18/18) | crash |

Key findings from that cross-app comparison (each independently confirmed,
not just asserted):
- **Not JUnit5-discovery-specific** — h2's harness uses plain
  `public static void main()` entry points, no JUnit Platform involvement
  at all, and still hit it. Tomcat uses JUnit4, not JUnit5, and hit it too.
- **Not a hard classpath-size gate** — h2's tiny classpath still triggered
  it, just much less often than the large-classpath apps.
- **Not always fatal** — only observed as a guarded recovery (ERROR logged,
  null substituted, execution continues) on h2; every large-classpath app
  saw a hard, deterministic crash before any test code ran. One h2 case
  (`TestCharsetCollator`) plausibly cascaded the guard's null substitution
  into a real downstream `ServiceConfigurationError`/NPE in
  `Charset$ExtendedProviderHolder.<clinit>` — meaning this class of guard
  hit can silently corrupt an otherwise-unrelated-looking test result, not
  just crash outright.

## The `dev` branch doesn't have it

A separate binary built from `origin/dev` (at the time, 79,851
insertions / 17,487 deletions ahead of the `main` commit above — mostly a
from-scratch `../../../../gc/src/zgc` rewrite, built in an isolated worktree purely to
get the `zgc` Cargo feature for a GC-algorithm comparison) showed **zero**
occurrences of this guard across ~30,000 class-runs: h2's full 218-class
suite × 3 GC variants, and Spring Framework's full 2848-class suite × 3 GC
variants, confirmed via `grep -c` over every complete `raw.log`.

## Merging `dev` into `main` and re-running confirms the fix, but explains only a small fraction of total failures

`main` was fast-forwarded to `681b5c1f1` (clean merge, zero conflicts —
`main`'s tip was already an ancestor of `origin/dev`), rebuilt
(`cargo build --release -p cratonvm-cli --features zgc`, 4m50s), and the
previously non-passing classes from both h2 and spring-framework's 3-way
GC sweep were re-run with the new binary.

**Zero** `HIB-CV-32`/corrupt-Value-cell hits across all 1239 reruns (both
exact-string and broad-pattern search over every log in all 6 output
directories) — the fix holds.

**But only 14 of 1239 previously-non-passing classes actually flip to
passing:**

| variant | rerun | newly passing |
|---|---:|---:|
| h2 default | 55 | 2 |
| h2 G1 | 57 | 2 |
| h2 ZGC | 57 | 2 |
| spring default | 359 | 2 |
| spring G1 | 351 | 0 |
| spring ZGC | 360 | 6 |

Newly-passing examples: h2's `TestMvccMultiThreaded`, `TestKillRestartMulti`,
`TestStringCache`; spring's `ClientHttpConnectorTests`, `RetryInterceptorTests`,
`ProceedTests`, `SqlUpdateTests`, `TransactionInterceptorTests`. Everything
else that was non-passing before the merge is *still* non-passing after —
mostly just shuffled between HANG/FAIL/CRASH flavors (e.g. some h2-G1
classes moved CRASH→HANG) — which is the expected shape for classes whose
real problem was never this guard in the first place. See:

- `gc-variant-fullsuite-crashes-hangs-fails-20260810-FIXED.md` (this folder) — what's
  still broken on h2 post-merge (a separate, still-open shared-SIGSEGV
  defect, plus a GC-specific object-grid-desync guard).
- `fixed-suite-bugs/spring/gc-variant-fullsuite-classpath-gap-and-fails-20260810-FIXED.md`
  — what was broken on spring-framework post-merge. RETIRED 2026-08-10: it was
  three defects, not the one harness classpath bug the original page described.
  Two were harness bugs (a `dumpTestCp` task that dumped jar paths it never
  built, and a JUnit Vintage deprecation notice promoted to fatal); the third
  was a real CratonVM defect (`sun.reflect.misc.MethodUtil` unloadable, taking
  the whole `javax.management` cluster with it). Re-measured after all three:
  87.5% → 99.0/99.2/99.1% OK.

One more defect that was live pre-merge and is confirmed **also gone**
post-merge, found while reading the postmerge h2 logs for this doc (not
mentioned in the original cross-app report, since it hadn't been checked
post-merge until now):

- The `sun.nio.ch.FileChannelImpl.tryLock`→`FileLockTable.add` NPE that was
  h2's dominant FAIL cause on file-backed MVStore opens — 0 occurrences in
  any postmerge log.

## What this doc does NOT establish

- **No regression check.** The rerun only touched previously-non-passing
  classes; none of the previously-PASS/OK classes (163+161+161 for h2,
  2489+2497+2488 for spring, ~7,500 classes total) were re-executed against
  the post-merge binary. A merge-induced regression among the
  previously-clean set would be invisible to everything measured here. If
  that assurance matters, it needs at least a sampled re-sweep of the
  previously-passing set.

  **Closed for spring-framework 2026-08-10**: the sweep recorded in
  `fixed-suite-bugs/spring/gc-variant-fullsuite-classpath-gap-and-fails-20260810-FIXED.md`
  re-ran all 2848 classes × 3 GC variants, not just the non-passing set, and
  found no regression among the previously-clean set (OK rose 2491 → 2819,
  2499 → 2824, 2494 → 2823). Still open for h2.
- **No bisection.** `dev` is 79k+ lines ahead of the pre-merge `main` tip;
  this doc did not narrow down which specific commit(s) fixed the guard.
  Given how tractable "confirm dev is clean, confirm main isn't, bisect
  between them" is compared to root-causing from scratch, that's the
  highest-leverage next step if a name/fix-commit is wanted for the
  changelog.

## Related

- `../../../known-issues/hibernate/g1-collector-fullsuite-crashes-hangs-fails-20260806.md` —
  the *original* HIB-CV-32 investigation this guard's log message cites;
  confirmed unrelated to the bug this doc describes beyond sharing a string.
- `../../../known-issues/hibernate/zgc-collector-fullsuite-crash-fails-20260806.md` — sibling
  ZGC investigation from the same day as the doc above.
