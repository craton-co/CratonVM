# `internal error: precise deoptimization unavailable ... refusing side-effecting replay` — cross-suite crash, every H2 CRASH + at least 1 Spring Framework crash

| | |
|---|---|
| **Status** | ✅ **FIXED 2026-09-07.** All 8 H2 CRASH classes stop crashing; the Spring side is closed by the same one-line predicate. |
| **Root cause** | The optimizing (IR) tier planted an unconditional uncommon trap at an `invokedynamic` it cannot lower, in methods whose earlier bytecode had already committed a side effect. That tier publishes no resumable deopt on a production artifact, so the interpreter's only fallback was a whole-method replay — which it correctly refuses once a side effect has been committed. |
| **Fix** | `IrBuilder::trap_replay_is_safe` (`jit/src/ir.rs`), consulted by `plant_uncommon_trap`. Kill switch `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0`. |
| **Measured** | Azure host `20.80.105.49`, worktree `/data/wt-l6-spring`, real JDK 25. |

## Result

All 8 classes this page named, run on one binary with only the guard's kill
switch varying:

| arm | PASS | FAIL | CRASH | `precise deoptimization unavailable` in any log |
|---|---:|---:|---:|---|
| guard ON (the fix) | 6 | 2 | **0** | **none** |
| `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | 0 | 0 | **8** | all 8 |

Per class, guard ON: `TestAlterSchemaRename` PASS, `TestTriggersConstraints`
PASS, `TestReorderWrites` PASS, `TestFreeSpace` PASS,
`TestKillProcessWhileWriting` PASS, `TestSampleApps` PASS, `TestFunctions`
FAIL, `TestBnf` FAIL.

The two remaining FAILs are pre-existing and were hidden behind the crash —
neither is this defect and neither is new:

* `org.h2.test.db.TestFunctions` — `testAnnotationProcessorsOutput`
  `AssertionError`. **HotSpot FAILs it identically** (re-measured today).
  Already recorded as
  `bug-h2-testannotationprocessorsoutput-jdk25-implicit-proc-disabled-NOT-A-BUG.md`.
* `org.h2.test.unit.TestBnf` — `testProcedures` `Expected: true got: false`;
  HotSpot PASSes. This is the already-tracked interpreter throughput-margin
  item against H2's hardcoded 100 ms `Sentence.MAX_PROCESSING_TIME`, open at
  `../../known-issues/h2/not-bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md`,
  which names `TestBnf.testProcedures()` explicitly as still failing.

## What this page got right, and the one thing it got wrong

Right, and load-bearing:

* `reason TransferToInterpreter` proves a deopt point **was** found at that
  bci — an empty set prints `UnreachedCode`.
* `can_deopt_resume=false (no deopt points, or an elided monitor)` is a
  disjunction the message cannot resolve for you.
* The IR tier's guards promise a precise resume that `can_deopt_resume` does
  not deliver in production, because it is set only behind
  `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`.
* "The right fix is to stop compiling these methods into an unresumable shape
  in the first place — the same strategy `ir_unresumable_protected_trap`
  already uses for its narrower case." That is exactly what was done.

Wrong, and worth recording because it cost the investigation its direction:
the page reasoned that because the application methods (`FilePathWrapper.wrap`,
two `toString()`s) are not obviously try/catch bodies, the trap must be
something other than `ir_unresumable_protected_trap`'s shape — perhaps the
"elided monitor" disjunct. **It is neither.** The trap is at an
`invokedynamic`, and every one of those three methods has one: a `toString()`
built by string concatenation compiles to
`invokedynamic makeConcatWithConstants`. Looking for a `try` block and finding
none ruled out one narrow shape and was read as ruling out the whole family.
`javap -c` on the trapping method, at the bci the message already printed,
would have said `invokedynamic` in one command.

That bci was in the error text the whole time: `at bci 21` for
`Scope$ScopeImpl.remove` is `invokedynamic #119`.

## The fix

`x64::bytecode_walk`'s single-pass `0xba` arm already refused to emit an
unresumable indy trap (`mark_codegen_unencodable("unresumable-indy-trap")`),
and its comment names this exact family — *"what the per-method
SPRING-TESTCOMPILER / HIB-STOREDPROC-JIT bans did by hand for the javac
family"*. The IR tier's `plant_uncommon_trap` cited the single-pass backend's
indy trade as its justification but did not copy that valve, and it needs one
more than the single-pass backend does, because its `can_deopt_resume` is
false on every production artifact.

`plant_uncommon_trap` now asks `trap_replay_is_safe` first, which is
`replay_from_entry_is_observably_equivalent`'s rule clause for clause — the
same "one predicate, asked at both ends" discipline the interpreter's own sink
documents. A refusal returns `false`, which the callers already turn into
`ir_build_bail`, so the method falls back to the single-pass backend rather
than going uncompiled.

Full derivation and the Spring-side evidence: the retired
`testcompiler-injit-mode-silent-compile-failure-19-class-aot-cluster` and
`jit-mode-explains-most-of-todays-56-class-fail-cluster` write-ups.

## Reproducing (historical)

```bash
cd apps/h2database-suite-runner
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-h2-suite.sh run \
  --only 'TestFreeSpace' --tag repro
# CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0 restores the crash on a fixed binary.
```

## Related

- `inline-trap-inside-a-protected-range-FIXED-20260818.md` — the narrower
  sibling this page correctly identified as the right strategy and wrongly
  ruled out as the right shape.
- `unresumable-unconditional-trap-mvmap-FIXED-20260802.md` — earlier sibling
  in the same "unresumable deopt" family.
