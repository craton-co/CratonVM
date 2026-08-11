# Linux full-suite residual — 9 classes pass cleanly on real HotSpot, fail on CratonVM

> **RESOLVED 2026-08-11 — this doc is retired.** All nine classes now pass on
> Azure Linux under the default collector at `--Xmx 1500m`, with
> found/ok/failed/aborted/skipped counts **byte-identical to real HotSpot** on
> every one. Four separate defects were behind them; none was the
> "G1-TLAB-adjacent / ANTLR native-root" family this doc guessed at, and the
> `JarVisitorTest` CRASH was not a VM bug at all.
>
> ## What each one turned out to be
>
> **1. The four HQL/query classes — a JIT inliner that pushed `null` for any
> `ldc` it could not describe.** `--nojit` was green and
> `CRATONVM_JIT_DENY=org/hibernate/dialect` was green, which put it inside one
> package; `CRATONVM_DBG_JIT_DISASM=extractPattern` then showed
> `H2Dialect.extractPattern` compiled to `xor eax,eax; ret` on the
> `unit != SECOND` path. The single-pass inline emitter materialises a callee's
> `ldc` as an x86 immediate, so `InlineSite::ldc_info` can only carry `Integer`
> and `Float` — and BOTH ends of that contract were fail-open. The resolver
> (`build_inline_site`) ended its constant-pool match `_ => 0`, recording a
> well-formed entry claiming a String's value was zero; the emitter's lookup
> ended `else { xor rax, rax }`. So the inlined body of the one-liner
> `Dialect.extractPattern(unit) { return "extract(?1 from ?2)"; }` returned
> `null`, and every HQL `extract()` / `cast()` / `str()` query died in
> `PatternRenderer.<init>` with
> `NullPointerException: … because "pattern" is null` — 17 of the 34 method
> failures directly, and the rest (`NoResultException`, wrong counts) as
> downstream effects of the mis-rendered SQL. Both ends now refuse instead: the
> resolver will not build a site it cannot describe, the emitter will not splice
> a body it cannot model, and `try_emit_inline`'s existing rollback makes the
> refusal a plain fallback to a real call.
> Guard: `s31_inline_refuses_an_ldc_it_has_no_constant_for` (three arms,
> verified red against the old code).
>
> **2. The two temporal HANGs — a ZGC conservative-root scan that walked the
> whole object-start bitmap per probe.** `perf record` on `InstantTests`:
> **80.7% of all CPU samples in `VmHeap::is_heap_addr`**, another 11.5% in
> `object_body_size` (the header dereference inside that walk).
> `ZgcRealHeap::is_heap_addr` is not a GC-only path — it backs per-slot
> conservative root scanning over ambiguous JVM-long-vs-jobject operand words —
> and any probe that missed the exact-base bitmap lookup but landed inside the
> arena envelope walked the bitmap forward from the arena base. Allocations do
> not overlap, so the only base whose extent can contain an address is the
> greatest base at or below it: `nearest_base_at_or_below` finds that with one
> backwards bit scan and one header dereference. `InstantTests` went from
> "not finished in 1800 s" to **52 s**.
> Guard: `nearest_base_at_or_below_matches_a_brute_force_walk`.
>
> **3. `SmokeTests` and `DefaultCatalogAndSchemaTest` — free-list holes capped
> at the ZGC TLAB chunk size.** Both died with
> `OutOfMemoryError: Java heap space` while the guard line read
> `free_list_bytes=1211993376 largest_free_block=65528`: 1.13 GiB free and no
> hole big enough for one 65552-byte `DFAState[8192]` out of ANTLR's
> `ParserATNSimulator`. On a non-compacting heap the TLAB chunk is the
> **granularity of free-list holes** — a chunk is one `arena.alloc`, and one
> survivor anywhere inside it walls it off from its neighbours — so
> `ZGC_TLAB_MAX_CHUNK = 64 KiB` was the worst available value: a 64 KiB hole is
> exactly too small for `new T[8192]` (`8192*8 + 16`). Confirmed by
> `CRATONVM_ZGC_TLAB=0` passing on the identical heap, and by G1 (which
> evacuates) passing. Raised to 512 KiB, which also lifts `max_tlab_alloc`
> (`chunk/8`) from 8 KiB to 64 KiB. Two supporting fixes landed with it:
> `Arena::alloc` now merges adjacent holes and retries before it will return
> `None` (coalescing previously ran only in a collector's post-sweep hook, while
> every split remainder and every TLAB retire minted un-merged adjacency), with
> a backoff so a heap whose holes are genuinely walled stops paying for the
> sort; and `alloc_raw` retires every TLAB and retries before raising.
> Guards: `alloc_merges_adjacent_holes_before_reporting_failure`,
> `coalesce_does_not_merge_across_a_gap`,
> `the_last_resort_merge_backs_off_when_merging_never_pays`.
>
> **4. `JarVisitorTest` — not a VM bug: a harness parse failure.** The class was
> passing 9/9 the whole time. `CratonRunner` writes `@@RESULT` to the same
> `System.out` the test bodies use, and `JarVisitorTest` ends with a
> newline-less `System.out.printf("InputStream byte[] extraction algorithms; …")`,
> so the run emitted ``…new = `17`@@RESULT org.hibernate…`` on one line and
> `run-hib.sh`'s anchored `grep "^@@RESULT "` found nothing — recorded as
> `CRASH process-died rc=0`. All five runner scripts now match the marker
> anywhere on the line; `run-hib.sh` additionally requires the class name it
> asked for. **rc=0 with no result line is a parse failure far more often than a
> VM failure.**
>
> ## Verified state (Azure Linux, default collector, `--Xmx 1500m`)
>
> | Class | CratonVM now | Real HotSpot |
> |---|---|---|
> | `query.hql.FunctionTests` | found=124 ok=118 failed=0 skipped=6, 78s | found=124 ok=118 failed=0 skipped=6 |
> | `type.temporal.ZonedDateTimeTest` | found=608 ok=404 failed=0 aborted=204, 381s | found=608 ok=404 failed=0 aborted=204, 17s |
> | `query.hql.StandardFunctionTests` | found=44 ok=44 failed=0, 41s | found=44 ok=44 failed=0 |
> | `type.temporal.InstantTests` | found=204 ok=112 failed=0 aborted=92, 52s | found=204 ok=112 failed=0 aborted=92, 6.7s |
> | `sql.exec.SmokeTests` | found=17 ok=16 failed=0 skipped=1, 322s | found=17 ok=16 failed=0 skipped=1, 10s |
> | `boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` | found=132 ok=132 failed=0, 2333s | found=132 ok=132 failed=0, 24s |
> | `hql.ASTParserLoadingTest` | found=106 ok=104 failed=0 skipped=2, 100s | found=106 ok=104 failed=0 skipped=2 |
> | `bootstrap.scanning.JarVisitorTest` | found=9 ok=9 failed=0, 11s | found=9 ok=9 failed=0, 1.9s |
> | `hql.HQLTest` | found=169 ok=168 failed=0 skipped=1, 38s | found=169 ok=168 failed=0 skipped=1 |
>
> ## What is NOT claimed
>
> Three of these are still far slower than HotSpot (`SmokeTests` 322s vs 10s,
> `ZonedDateTimeTest` 381s vs 17s, `DefaultCatalogAndSchemaTest` 2333s vs 24s)
> and after the `is_heap_addr` fix the profile is flat — no single remaining
> hotspot, i.e. a general throughput gap rather than another defect of this
> shape. All three exceed the suite's flat 300s per-class cap, so
> `class-overrides.tsv` carries a documented timeout FLOOR for each (900s /
> 900s / 3600s); without it a full-suite run reports them HANG again for a
> reason that has nothing to do with correctness. `SmokeTests`'s entry is new
> with this fix.
>
> The historical investigation below is preserved as written. Note that its
> cross-reference section guessed wrong on three of the four causes: this is not
> the G1-TLAB family, `DefaultCatalogAndSchemaTest` is not the
> `StringWriter`/old-gen-arena gap it was attributed to (it is the same TLAB
> hole-granularity bug as `SmokeTests`), and `JarVisitorTest` is not a VM defect
> at all — though the doc's own suggestion to look for something
> "short-circuiting the harness" rather than a VM bug was the right instinct.

**Status:** RESOLVED (2026-08-11); originally filed OPEN the same day. Azure Linux host, worktree
`/data/cratonvm-hib-linux-20260811` (branch `test/hib-linux-20260811`, dev
tip `54ce35df9` plus the local `run-hib.sh` portability fix), default
collector, H2 in-memory DB, real JDK 25.

## Method

After fixing the false-positive bytecode-enhancement sysprop gap (see
companion doc), the full 4579-class suite left 10 non-passed classes (7
FAIL, 2 HANG, 1 CRASH). Each was re-run individually under **real HotSpot**
(`/data/toolchain/jdk-25/bin/java`, same `common.args` classpath and
sysprops CratonVM uses, same `CratonRunner` JUnit5 launcher — a plain
vanilla `DiscoverySelectors.selectClass` + `LauncherFactory` call, nothing
CratonVM-specific) to confirm each failure is a genuine CratonVM-vs-HotSpot
divergence and not a pre-existing, environment-wide issue.

**One of the 10 turned out NOT to be a CratonVM bug** —
`ProxyPreservingFiltersOutsideInitialSessionTest` fails identically on both
VMs (`found=4 ok=1 failed=1 skipped=2` on both CratonVM and HotSpot) — byte-for-byte
matching counts, so it's excluded from the table below and not tracked
here.

## Confirmed genuine divergences (9 classes)

| Class | CratonVM | Real HotSpot (JDK 25) |
|---|---|---|
| `query.hql.FunctionTests` | FAIL: found=124 ok=94 **failed=24** skipped=6 | found=124 ok=118 failed=0 skipped=6 |
| `type.temporal.ZonedDateTimeTest` | **HANG**: rc=124, timeout=900s | found=608 ok=404 failed=0 aborted=204, ms=17051 |
| `query.hql.StandardFunctionTests` | FAIL: found=44 ok=39 **failed=5** | found=44 ok=44 failed=0 |
| `type.temporal.InstantTests` | **HANG**: rc=124, timeout=300s | found=204 ok=112 failed=0 aborted=92, ms=6708 |
| `sql.exec.SmokeTests` | FAIL: **found=0** failed=1 (class-level failure, no tests even started) | found=17 ok=16 failed=0 skipped=1, ms=4669 |
| `boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` | FAIL: found=0 failed=1, ms=618664 (10+ min before failing) | found=132 ok=132 failed=0, ms=24278 |
| `hql.ASTParserLoadingTest` | FAIL: found=106 ok=102 **failed=2** skipped=2 | found=106 ok=104 failed=0 skipped=2 |
| `bootstrap.scanning.JarVisitorTest` | **CRASH**: rc=0, no `@@RESULT` line printed despite a clean exit code | found=9 ok=9 failed=0, ms=1907 |
| `hql.HQLTest` | FAIL: found=169 ok=165 **failed=3** skipped=1 | found=169 ok=168 failed=0 skipped=1 |

## Cross-references to prior same-day findings

Most of these are not new defects in isolation — they match root causes
already under active investigation elsewhere this session, now confirmed to
reproduce on Linux too (same VM binary, different OS):

- **`DefaultCatalogAndSchemaTest`** — the long-running, still-open
  non-compacting-old-gen-arena fragmentation gap (see the Windows-side
  investigation the same day: the proxy-dispatch `Method`-object cache
  landed as a real fix for 9 other classes but does not resolve this one;
  root cause is a `StringWriter` buffer grow hitting fragmented old-gen
  space during DDL script accumulation, unrelated to the proxy fix).
- **`FunctionTests` / `ASTParserLoadingTest` / `HQLTest` / `StandardFunctionTests`**
  — all in the HQL-parser/query family already flagged this session as a
  recurring cluster (G1 TLAB-alignment-adjacent symptoms, ANTLR native-root
  interactions). Worth checking whether `79c302916` (the G1 TLAB fix) or a
  sibling fix touches these under the *default* collector too — these
  residuals are under the default collector here, not G1, so if the G1 fix
  doesn't apply this may be a distinct default-collector-only shape of the
  same family.
- **`ZonedDateTimeTest` / `InstantTests`** — join the existing temporal-type
  HANG family (`OffsetDateTimeTest`/`OffsetTimeTest` already have timeout
  floors in `class-overrides.tsv`); `InstantTests` in particular is new to
  this specific family and not previously seen hanging.
- **`SmokeTests`** — the class itself is a long-tracked residual
  (`testQueryConcurrency` throughput timeout, documented and RETIRED
  2026-07-23), but the *shape* here (`found=0`, a class-level failure before
  any test starts) does not match that already-retired doc's shape
  (`found=17 failed=1`, one test method timing out). Worth a fresh look —
  this may be a different failure on the same class, not a recurrence of
  the retired one.
- **`JarVisitorTest`** — genuinely new: `CRASH` with `rc=0` (a clean process
  exit code) but no `@@RESULT` line printed. Every other CRASH this session
  has been a nonzero rc (SIGSEGV, timeout). An rc=0 CRASH means the process
  exited normally but never reached the point in `CratonRunner.main` that
  prints the result line — worth checking whether this is a
  `System.exit()` called from deep inside the test itself (JarVisitorTest
  manipulates JAR/classpath scanning, a plausible place for a stray
  `System.exit`) short-circuiting the harness, rather than a VM bug.

## Repro (per class)

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home /data/toolchain/jdk-25
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner <className>
# control:
/data/toolchain/jdk-25/bin/java @common.args -Dcraton.batch=1 CratonRunner <className>
```
Common args must include the bytecode-enhancement sysprop fix from the
companion doc, or every class in this suite fails for an unrelated reason.

## Related

- `azure-linux-common-args-missing-bytecode-enhancement-sysprop-20260811-FIXED.md`
  — the harness config gap that produced the other 296 false positives this
  same run.
