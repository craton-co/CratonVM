# `internal error: precise deoptimization unavailable ... refusing side-effecting replay` — cross-suite crash, every H2 CRASH + at least 1 Spring Framework crash

| | |
|---|---|
| **Status** | **FIXED 2026-09-07** (`CRATONVM_JIT_DEOPT_SINK_RESUME`, default ON). Was: OPEN, high severity — hard process abort (`InternalError`), not a catchable exception. |
| **The fix** | `deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md` carries the mechanism, the A/B and the in-repo witness. This page keeps the H2 + Spring population that found it. |
| **Verified on H2** | Yes — all 8 CRASH classes re-run on a current binary, **0 occurrences of the abort**. See *Verified on the H2 CRASH population itself*. |
| **Scope** | All 8 distinct CRASH classes across the 2026-09-07 full 218-class 3-GC-arm H2 run's three completed arms (Generational CRASH=8, G1 CRASH=8, ZGC CRASH=5 — 8 unique classes total, listed below), plus at least one Spring Framework class from the same day's run. **This is the entire H2 CRASH population for this run — no other crash mechanism was found.** |

## Symptom

Every affected class dies with a hard, uncatchable process abort:

```
Exception in thread "main" java/lang/InternalError: JIT dispatch into
  <method> failed: internal error: precise deoptimization unavailable for
  <method> at bci <N> (can_deopt_resume=false (no deopt points, or an
  elided monitor), stashed key "<method>", inline callers 0,
  reason TransferToInterpreter); refusing side-effecting replay
```

Confirmed instances today (all 8 CRASH classes, final complete 3-arm data):

| class | trapping method | arms hit |
|---|---|---|
| `org.h2.test.store.TestKillProcessWhileWriting` | `org/h2/store/fs/FilePathWrapper.wrap(Lorg/h2/store/fs/FilePath;)Lorg/h2/store/fs/FilePathWrapper;` | Gen, G1, ZGC |
| `org.h2.test.store.TestFreeSpace` | `org/h2/test/store/FreeSpaceList$BlockRange.toString()Ljava/lang/String;` | Gen, G1, ZGC |
| `org.h2.test.poweroff.TestReorderWrites` | `org/h2/test/utils/FileReorderWrites$FileWriteOperation.toString()Ljava/lang/String;` | Gen, G1, ZGC |
| `org.h2.test.unit.TestSampleApps` | `com/sun/tools/javac/code/Scope$ScopeImpl.remove(Lcom/sun/tools/javac/code/Symbol;)V` (same javac method as below) | Gen, G1, ZGC |
| `org.h2.test.db.TestFunctions` | `com/sun/tools/javac/code/Scope$ScopeImpl.remove(Lcom/sun/tools/javac/code/Symbol;)V` (**inside H2's own in-process javac**, via `SourceCompiler.javaxToolsJavac` compiling a `CREATE ALIAS ... AS $$ ... $$` function body) | Gen, G1, ZGC |
| `org.h2.test.db.TestTriggersConstraints` | same javac method | Gen, G1 only |
| `org.h2.test.db.TestAlterSchemaRename` | same javac method | Gen, G1 only |
| `org.h2.test.unit.TestBnf` | same javac method (confirmed) | Gen, G1 only |

Collector-independence is not clean here — 3 classes (`TestTriggersConstraints`,
`TestAlterSchemaRename`, `TestBnf`) crashed on Generational and G1 but passed
on ZGC in this run. Given the trap is timing/JIT-warmup-sensitive (it only
fires once a specific method gets hot enough to reach the optimizing tier),
this is consistent with the same underlying bug being probabilistic across
runs rather than evidence of a second, GC-specific mechanism — not confirmed
either way in this session.
**All 6 of today's H2 CRASH classes share this one mechanism** — there is no
separate H2 crash cluster to triage, just this one bug hitting 6 different
call sites (three application-level `toString()`/`wrap()` methods, three via
javac's internals).

## Not a new mechanism — a known-open gap in an already-"fixed" class of bug

`docs/internal/fixed-bugs/inline-trap-inside-a-protected-range-FIXED-20260818.md`
already root-caused the *shape* of this error precisely:

- `reason TransferToInterpreter` in the message is proof a deopt point
  **was found** at that bci — not, as an earlier revision of that page
  wrongly concluded, evidence of "no deopt points at all". (An empty deopt-point
  set prints `reason UnreachedCode`, not `TransferToInterpreter`.)
- `can_deopt_resume=false (no deopt points, or an elided monitor)` is a
  **disjunction of two distinct causes**, not a single fact — the message
  format cannot by itself tell you which one applies.
- The IR (optimizing) tier's deopt guards (`emit_array_null_bounds_guards`,
  `emit_deopt_if_zero` in `jit/src/ir_lower.rs`) promise a **precise resume**
  (re-execute one opcode) but `can_deopt_resume` defaults to `false` and is
  only set `true` behind `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL` —
  flags production builds do not set. So any IR-tier-compiled method that
  hits one of these guards **cannot** keep the promise, and the VM correctly
  refuses an imprecise resume (which risks a wrong answer) rather than
  silently doing it — hence the hard abort instead of a normal exception.

That page's fix, `ir_unresumable_protected_trap`, declines optimizing-tier
admission for methods whose deopt-guarded opcode sits **inside a protected
(try/catch) range that also commits a side effect before the trap** —
falling back to the single-pass backend, which never needs a precise resume
at all (it throws directly through the exception table).

**Today's crashes are not necessarily inside a protected range** — none
of the application methods (`wrap`, two `toString()`s) are obviously
try/catch bodies, and the message's own "no deopt points, **or an elided
monitor**" wording names a second cause the 2026-08-18 fix's admission gate
does not obviously address. Whether these are the "elided monitor" branch, a
different unresumable-trap shape entirely, or a scenario the existing
`ir_unresumable_protected_trap` check should catch but doesn't, was **not
determined in this session**.

## Full 3-arm H2 run totals, for context

| arm | PASS | FAIL | CRASH | HANG | wall |
|---|---:|---:|---:|---:|---|
| Generational | 145 | 18 | 8 | 47 | 5h44m |
| G1 | 146 | 19 | 8 | 45 | 5h32m |
| ZGC | 150 | 17 | 5 | 46 | 5h32m |

Every CRASH in every arm is one of the 8 classes above — this single JIT
defect is the entire H2 CRASH story for this run, not a sample of a larger
unexplained population.

## Cross-suite: also seen in Spring Framework today

The identical wording (`internal error: precise deoptimization unavailable
for ... refusing side-effecting replay`) appeared in the same day's Spring
Framework 3-GC-arm run, in `CrossOriginAnnotationIntegrationTests`, trapping
in `org/springframework/web/cors/CorsConfiguration.addAllowedOriginPattern`
and `.addAllowedOrigin` — see
`docs/known-issues/spring/jit-mode-explains-most-of-todays-56-class-fail-cluster-20260907.md`.
Different methods, same mechanism, same day, two unrelated test suites. This
is not a suite-specific defect — it is a general JIT correctness/robustness
gap that any sufficiently-hot method with the right shape can hit.

## Why this matters more than a normal FAIL

This is a **hard process abort**, not a catchable test failure — every class
that hits it loses all its remaining test methods for that run, and (per the
architecture doc's own framing) the alternative to aborting would be an
imprecise resume that risks silently computing a **wrong answer**. The abort
is the VM correctly refusing to guess, but the right fix is to stop compiling
these methods into an unresumable shape in the first place — the same
strategy `ir_unresumable_protected_trap` already uses for its narrower case.

## Not done in this session

- Did not determine whether these six sites are inside a protected range
  (the existing fix's exact trigger condition) or hit the "elided monitor"
  disjunct, or something else. `CRATONVM_DBG_JITC=1` on one of the H2 repros
  (`TestFreeSpace` is the smallest/fastest) would show the admission decision
  for `FreeSpaceList$BlockRange.toString()`.
- Did not confirm `--nojit` clears these (very likely, given the error is a
  JIT-only code path, but not verified this session for H2 specifically —
  it was verified for the analogous Spring Framework case).
- javac's own `Scope$ScopeImpl.remove` being a repeat offender across three
  independent H2 classes suggests it's simply a hot, commonly-JIT-compiled
  method inside any workload that runs H2's in-process javac (`CREATE ALIAS`,
  triggers, or schema DDL that compiles Java source) — worth checking whether
  disabling JIT specifically for the forked/nested javac invocation (as
  opposed to the whole VM) would be a viable narrower mitigation.

## Reproducing

```bash
cd apps/h2database-suite-runner
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-h2-suite.sh run \
  --only 'TestFreeSpace' --tag repro         # crashes, InternalError
```

## Related

- `inline-trap-inside-a-protected-range-FIXED-20260818.md` — the
  architecturally-identical, narrower-scoped fix that does not cover these
  six sites.
- `unresumable-unconditional-trap-mvmap-FIXED-20260802.md` — an earlier
  sibling in the same "unresumable deopt" family, worth checking for the same
  reason.
- `jit-mode-explains-most-of-todays-56-class-fail-cluster-20260907.md` — the
  Spring Framework side of today's finding.

## Verified on the H2 CRASH population itself (2026-09-07, later)

This page was retired on the strength of an in-repo witness reproducing the
signature exactly and of the mechanism being one gate. **It was not verified on
H2**, because the suite's test classes were not compiled on this host — which
left the page's central claim ("all 8 CRASH classes, this one mechanism")
resting on inference. Closed now.

All eight were compiled from the checkout's OWN sources
(`javac -sourcepath "src/main;src/test;src/tools"`, 1 527 classes, zero errors)
and run on a current `dev` binary:

| class | exit | `precise deoptimization unavailable` |
|---|---|---:|
| `store.TestFreeSpace` | normal | **0** |
| `poweroff.TestReorderWrites` | normal | **0** |
| `db.TestAlterSchemaRename` | normal | **0** |
| `db.TestTriggersConstraints` | normal | **0** |
| `store.TestKillProcessWhileWriting` | normal | **0** |
| `unit.TestSampleApps` | failed | **0** |
| `db.TestFunctions` | failed | **0** |
| `unit.TestBnf` | failed | **0** |

**Zero occurrences of the abort across the whole population.** Five of the eight
now run to a normal exit; the three that do not fail on something else
entirely — a timezone comparison (`Expected: UTC (3) actual: America/…`), an
`IOException`/NPE out of the sample-app driver, and a `testProcedures`
assertion. None is this signature, and all three are consistent with the
ad-hoc build used here rather than with a VM defect: they are exactly the
`CREATE ALIAS`-driven classes, whose in-process javac wants a real classpath,
and this run had no suite harness supplying one.

**So: the crash mechanism is gone, and the pass/fail status of those three is
not established here.** Settling that needs `mvnw test-compile` and the suite
runner; the offline attempt failed on a missing `org.ow2.asm:asm-bom:9.5`.

### Why the first attempt did not count

Compiling the test sources against the published `h2-2.4.240.jar` looked
easier and produced a nine-build-ID version skew — the checkout is
`2.4.249-SNAPSHOT`. Two classes failed under it that pass without it
(`TestTriggersConstraints`, `TestKillProcessWhileWriting`), so that arm's
failures were the skew talking. Building main and test from the same tree is
what makes the table above mean anything.

## Answered (2026-09-07)

The "Not done in this session" list above is closed by
`deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md`:

* **Protected range, or the "elided monitor" disjunct, or something else?**
  **Something else, and the question was the wrong one.** Neither disjunct
  applies. `can_deopt_resume` is false on EVERY optimizing-tier artifact in a
  production build, for a reason that has nothing to do with either the deopt
  points or a monitor: `ir_lower` sets it only inside a condition requiring
  `sr_map.is_some()`, and `sr_map` is populated only under
  `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`. The message's parenthetical
  named two causes, and the actual cause was neither — which is why chasing
  the disjunction would not have converged. The refusal message now says which
  gate actually declined.
* **Does `--nojit` clear these?** Necessarily. The whole path is JIT-only: no
  compiled body, no trap, no stash, no sink.
* **`Scope$ScopeImpl.remove` as a repeat offender across three H2 classes.**
  Exactly as suspected — a hot, commonly-compiled method inside any workload
  that runs H2's in-process javac. It needed no narrower mitigation; it is the
  same one gap, hit from three call sites.

The trapping methods are also explained rather than merely listed: all three
application-level entries (`FilePathWrapper.wrap`, two `toString()`s) and the
javac ones reach an optimizing-tier body that traps — for a `toString()` built
by string concatenation, at the `invokedynamic makeConcatWithConstants` the
optimizing tier plants an unconditional uncommon trap at
(`ir::ir_site_trap_enabled`, default ON). That is why the trap is not rare: it
fires the first time the compiled body reaches the site.

## Also answered: the ZGC asymmetry

Three classes crashed on Generational and G1 and passed on ZGC, and this page
declined to call that GC-independence. It is not GC-dependence: the trap fires
only once a method reaches the optimizing tier, and which methods get there by
a given point in a run is timing-sensitive. The hibernate-reactive population
(`jit-precise-deopt-refused-transfer-to-interpreter-hibreactive-20260907-FIXED.md`)
saw the identical signature on all three collectors in the same run, same host,
same binary — which is the cleaner reading of the same mechanism.
