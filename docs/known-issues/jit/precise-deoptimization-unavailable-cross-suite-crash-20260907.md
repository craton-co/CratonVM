# `internal error: precise deoptimization unavailable ... refusing side-effecting replay` — cross-suite crash, all 6 of today's H2 CRASHes + at least 1 Spring Framework crash

| | |
|---|---|
| **Status** | OPEN. High severity — hard process abort (`InternalError`), not a catchable exception. |
| **Scope** | 6 of 6 H2 CRASH classes in the 2026-09-07 full 218-class 3-GC-arm run (`TestKillProcessWhileWriting`, `TestFreeSpace`, `TestReorderWrites`, `TestFunctions`, `TestTriggersConstraints`, `TestAlterSchemaRename`), plus at least one Spring Framework class (`TestKillProcessWhileWriting`'s log shows the identical mechanism firing inside `TestKillProcessWhileWriting.main` itself, and the same wording appears in a Spring Framework JIT error captured separately today). |

## Symptom

Every affected class dies with a hard, uncatchable process abort:

```
Exception in thread "main" java/lang/InternalError: JIT dispatch into
  <method> failed: internal error: precise deoptimization unavailable for
  <method> at bci <N> (can_deopt_resume=false (no deopt points, or an
  elided monitor), stashed key "<method>", inline callers 0,
  reason TransferToInterpreter); refusing side-effecting replay
```

Confirmed instances today:

| class | trapping method |
|---|---|
| `org.h2.test.store.TestKillProcessWhileWriting` | `org/h2/store/fs/FilePathWrapper.wrap(Lorg/h2/store/fs/FilePath;)Lorg/h2/store/fs/FilePathWrapper;` |
| `org.h2.test.store.TestFreeSpace` | `org/h2/test/store/FreeSpaceList$BlockRange.toString()Ljava/lang/String;` |
| `org.h2.test.poweroff.TestReorderWrites` | `org/h2/test/utils/FileReorderWrites$FileWriteOperation.toString()Ljava/lang/String;` |
| `org.h2.test.db.TestFunctions` | `com/sun/tools/javac/code/Scope$ScopeImpl.remove(Lcom/sun/tools/javac/code/Symbol;)V` (**inside H2's own in-process javac**, via `SourceCompiler.javaxToolsJavac` compiling a `CREATE ALIAS ... AS $$ ... $$` function body) |
| `org.h2.test.db.TestTriggersConstraints` | same javac method |
| `org.h2.test.db.TestAlterSchemaRename` | same javac method |

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

**Today's six crashes are not necessarily inside a protected range** — none
of the three application methods (`wrap`, two `toString()`s) are obviously
try/catch bodies, and the message's own "no deopt points, **or an elided
monitor**" wording names a second cause the 2026-08-18 fix's admission gate
does not obviously address. Whether these are the "elided monitor" branch, a
different unresumable-trap shape entirely, or a scenario the existing
`ir_unresumable_protected_trap` check should catch but doesn't, was **not
determined in this session**.

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
