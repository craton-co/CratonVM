# `org.springframework.core.test.tools.TestCompiler` silently fails to compile under JIT — 19-class AOT cluster

| | |
|---|---|
| **Status** | ✅ **FIXED 2026-09-07.** `TestCompilerTests` is `found=22 succ=22 fail=0` under JIT-on; every class in the cluster passes. |
| **Root cause** | The optimizing (IR) tier planted an uncommon trap at an `invokedynamic` it cannot lower, in a method whose earlier bytecode had already committed a side effect — and that tier publishes no resumable deopt on a production artifact, so the trap was a guaranteed `InternalError` rather than a slow path. |
| **Fix** | `IrBuilder::trap_replay_is_safe` (`jit/src/ir.rs`), consulted by `plant_uncommon_trap`. Kill switch `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0`. |
| **Scope** | 19 of the 56 classes common to all three GC arms in the 2026-09-07 full 2848-class Spring Framework suite run — every class that exercises `TestCompiler`. The same defect accounts for the other 37; see the sibling page. |
| **Measured** | Azure host `20.80.105.49`, worktree `/data/wt-l6-spring`, branch `fix/spring-jit-aot-clusters-20260907`, real JDK 25. |

## What the failure actually was

`TestCompiler.compile()` reported `task.call() == false` with **zero**
diagnostics through the `DiagnosticListener`, which is why the original page
read it as a silent failure "inside the compile task itself". It was not
silent — javac printed its own crash banner to `stderr`, which the harness
captured but the `CompilationException` message could not carry:

```
An exception has occurred in the compiler (25.0.4). ...
java.lang.InternalError: JIT dispatch into
  com/sun/tools/javac/code/Scope$WriteableScope.remove(Lcom/sun/tools/javac/code/Symbol;)V
  failed: internal error: precise deoptimization unavailable for
  com/sun/tools/javac/code/Scope$ScopeImpl.remove(Lcom/sun/tools/javac/code/Symbol;)V
  at bci 21 (can_deopt_resume=false (no deopt points, or an elided monitor),
  stashed key "com/sun/tools/javac/code/Scope$ScopeImpl.remove:(...)V",
  inline callers 0, reason TransferToInterpreter);
  refusing side-effecting replay
	at com.sun.tools.javac.code.Symtab.enterClass(Symtab.java:703)
	...
```

javac catches `Throwable` around its own compile, prints that banner, and
returns `false`. A `DiagnosticListener` never sees it. **A `task.call()` that
returns falsy with an empty diagnostic collector is a reason to go read
stderr, not a reason to conclude the failure had no message.**

## Root cause

`com.sun.tools.javac.code.Scope$ScopeImpl.remove` is 200-odd bytes with two
relevant instructions:

```
   12: invokestatic  #20   // Assert.check:(Z)V          <- a side effect
   21: invokedynamic #119  // Predicate for lookup(...)  <- the trap site
```

The optimizing tier has no lowering for a bootstrap call site, so
`IrBuilder`'s `0xba` arm plants an unconditional uncommon trap at bci 21 and
compiles the rest of the method (`[ir] site TRAP planted at bytecode pc 21`).
The single-pass backend has made the same trade since it stopped bailing on
indy — and the IR arm's own doc comment cites that as its justification.

**But the single-pass backend carries a safety valve the IR arm did not
copy.** `x64::bytecode_walk`'s `0xba` arm builds a snapshot, checks
`frame_state_is_resumable`, and bails the entire compile
(`mark_codegen_unencodable("unresumable-indy-trap")`) when the trap it is
about to emit could not be resumed. Its comment even names this family:
*"this is what the per-method SPRING-TESTCOMPILER / HIB-STOREDPROC-JIT bans
did by hand for the javac family."*

The IR tier had no equivalent, and it needs one more than the single-pass
backend does: `x64::driver` sets
`can_deopt_resume = !deopt_points.is_empty() && !has_elided_monitor`, while
`ir_lower` sets it **only** on the scalar-replacement path
(`sr_map.is_some()`, i.e. `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`).
On a production artifact an optimizing-tier deopt therefore has exactly one
fallback: the interpreter's whole-method replay from entry. And that replay is
refused — fatally — whenever the bytecode before the trap already committed
something a re-run would duplicate. `Assert.check` at bci 12 is such a thing.

So: **every** execution of the compiled `ScopeImpl.remove` that reached bci 21
raised `InternalError`. It is not intermittent and not a miscompile; the trap
is unconditional and the refusal is the VM correctly declining to guess.

This is the general shape of one tier copying another tier's trade-off
without copying its precondition.

## The fix

`plant_uncommon_trap` now refuses to plant when the trap would not be
resumable, and a refusal returns `false`, which its callers already turn into
`ir_build_bail` — the method falls back to the single-pass backend (whose own
indy check then applies) rather than being left uncompiled.

The predicate asked at the producing end is the CONSUMER's, clause for
clause: `IrBuilder::trap_replay_is_safe` mirrors
`replay_from_entry_is_observably_equivalent`
(`vm/src/runtime/interpreter/deopt_resume.rs`) — whole body pure, else every
spliced body pure and the prefix before the trap pure. That is the same
"one predicate, asked at both ends" discipline `opcode_commits_side_effect`
already documents; a paraphrase is exactly how these two ends drifted apart
in the first place.

One consequence is worth knowing when reading the code: `invokedynamic` is
`0xba` and `opcode_commits_side_effect` commits the whole `0xb6..=0xba`
invoke range, so **a body containing an indy can never satisfy the
whole-body clause** — every indy trap is decided by the prefix clause alone.
`clause_one_can_never_fire_for_an_invokedynamic_body` asserts this so that
widening or narrowing the opcode set says so at the test rather than quietly
changing which methods get trapped.

`CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` restores the old behaviour in the same
binary; `ir_trap_refusal_census()` counts refusals by cause beside
`ir_trap_census()`'s plants, so the cost of the guard is countable rather than
argued about.

## Evidence

A 98-line standalone reproducer (`JavacLoopProbe.java`: repeated in-process
javac through an in-memory `JavaFileManager`, the shape `TestCompiler` uses)
was validated on HotSpot first, then run four ways:

| arm | result |
|---|---|
| HotSpot JDK 25 | 20 iterations, 0 failures |
| CratonVM, `dev` @ `e90fa0274` | **20 of 20 failed**, first failure at iteration 0 |
| CratonVM, fixed | 20 iterations, 0 failures |
| CratonVM, fixed, `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | **20 of 20 failed** |

The last two rows are one binary, so the guard is the only variable.
`CRATONVM_DBG_IR_COMPILES=1` reports 120 `site TRAP REFUSED` lines in a
three-iteration run.

Real suite, `TestCompilerTests` alone under `run-suite.sh run`:

| | found | succ | fail |
|---|---:|---:|---:|
| before | 22 | 3 | 19 |
| after | 22 | 22 | 0 |

The whole 19-class cluster is covered by the sibling page's 56-class run.
`TestClassScannerTests` and `HttpServiceProxyRegistrationAotProcessorTests`,
listed in the original page as unconfirmed members, both pass (`7/7` and
`5/5`).

## What the guard costs

Nothing measurable, and on this workload it is a small **win** — which is the
expected sign once the mechanism is clear. A planted indy trap is
*unconditional*: `x64::driver`'s own note says "a compiled 0xba site is an
UNCONDITIONAL trap (the instruction is never JIT-executed), so any execution of
this artifact that reaches it deopts". An optimizing body whose live path runs
into one is therefore strictly worse than the single-pass body it superseded —
it pays a deopt on every call. Declining it hands the method back to a tier
that runs it.

24 Spring Framework classes (673 test methods) that pass in both arms, one
binary, ABBA-interleaved to keep host drift off the comparison:

| slot | arm | sum-class-ms |
|---:|---|---:|
| 1 | guard ON (default) | 294 876 |
| 2 | `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | 316 441 |
| 3 | `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | 336 893 |
| 4 | guard ON (default) | 286 554 |

Both guard-ON slots sit below both guard-OFF slots with no overlap, and the
*last* slot is the fastest of the four — so a monotone host-load drift cannot
produce this ordering. Mean 290.7 s vs 326.7 s, ~11 % in the guard's favour.

Refusal volume, for scale: the javac loop probe reports 122 `site TRAP REFUSED`
against 13 `site TRAP planted` in five iterations (`CRATONVM_DBG_IR_COMPILES=1`).
A refusal is `ir_build_bail`, which falls back to the **single-pass backend**,
not to the interpreter — so a refused method is still compiled.
`ir_trap_refusal_census()` reports these counts by cause beside
`ir_trap_census()`'s plants.

## What this did NOT fix

`org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests`
is in this cluster and now passes **14 of 14, matching HotSpot** — but it
takes 1067 s where HotSpot takes 10.7 s, so it still reads as `TIMEOUT` at the
suite's default 180 s per-class cap. That throughput gap is its own open page,
`beanregistrations-verylarge-throughput-20260907.md` in the Spring
known-issues folder; before this fix the class failed too early to expose it.

## Relationship to the already-fixed sibling bug

`aot-cglib-dynamicclassfileobject-illegalargumentexception-20260811-FIXED.md`
shares the architecture (`DynamicJavaFileManager` / in-memory compile) and the
original page asked whether this was a residual of that defect. It is not:
that one was the JIT resolving a callee by class name across loaders; this one
is a trap-resumability gap in a different backend and would have hit any
method with the same shape, in any workload. The H2 side of the same defect
(three application `toString()`/`wrap()` methods, no javac involved) is the
clearest evidence the two are unrelated — see the retired
`precise-deoptimization-unavailable-cross-suite-crash` write-up.

## Reproducing (historical)

```bash
cd apps/spring-suite-runner
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-suite.sh run \
  --only 'TestCompilerTests$' --tag repro
# before: found=22 succ=3 fail=19; after: 22/22/0
# CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0 restores the failure on a fixed binary.
```
