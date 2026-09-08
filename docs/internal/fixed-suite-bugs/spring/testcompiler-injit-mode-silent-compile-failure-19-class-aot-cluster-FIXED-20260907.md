# `org.springframework.core.test.tools.TestCompiler` silently fails to compile under JIT — 19-class AOT cluster

| | |
|---|---|
| **Status** | ✅ **FIXED 2026-09-07.** `TestCompilerTests` is `found=22 succ=22 fail=0` under JIT-on; every class in the cluster passes. |
| **Root cause** | The optimizing (IR) tier planted an uncommon trap at an `invokedynamic` it cannot lower, in a method whose earlier bytecode had already committed a side effect — and no consumer of that trap could both resume it precisely and refuse to duplicate the side effect. |
| **Fixed by two independent changes the same day** | The abort was closed by the sibling sink fix (`deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md`), which makes `execute`'s tier-up sink resume the frame instead of raising `InternalError`. The trap is no longer planted at all by `IrBuilder::trap_replay_is_safe` (`jit/src/ir.rs`), which also closes the silent half the resume fix does not reach. Kill switch `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0`. |
| **Scope** | 19 of the 56 classes common to all three GC arms in the 2026-09-07 full 2848-class Spring Framework suite run — every class that exercises `TestCompiler`. |
| **Measured** | Azure host `20.80.105.49`, worktree `/data/wt-l6-spring`, real JDK 25. |

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
On a production artifact an optimizing-tier deopt therefore had exactly one
fallback: the interpreter's whole-method replay from entry. And that replay is
refused — fatally — whenever the bytecode before the trap already committed
something a re-run would duplicate. `Assert.check` at bci 12 is such a thing.

So **every** execution of the compiled `ScopeImpl.remove` that reached bci 21
raised `InternalError`. It is not intermittent and not a miscompile; the trap
is unconditional and the refusal was the VM correctly declining to guess.

This is the general shape of one tier copying another tier's trade-off
without copying its precondition.

## Two fixes, and which one closed what

Both landed on `dev` on 2026-09-07, from two lanes that found the same defect
from opposite ends. They are not redundant.

**The sink fix** (`deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md`)
made `execute`'s tier-up sink resume the stashed frame precisely instead of
aborting — the same `build_deopt_frame_inner` its sibling
`try_resume_trapped_callee` already used. That closes the `InternalError`,
and on its own it makes this page's symptom go away: the javac probe below
passes with the compiler-side guard switched off.

**The compiler-side refusal** (this page's other half) stops the trap being
planted at all, and closes a hazard the resume fix does not reach — which
turns out to be most of the damage. Two other sinks consume the same stash —
`jit_bridge`'s `jit-callsite-a` / `jit-callsite-b`, which a method reaches
when it is called from ordinary bytecode *after* it already has an artifact —
and those **re-run the whole method from entry**, silently.
`jit-bridge-sinks-re-run-a-side-effecting-body-20260907.md` filed that as an
open hazard with no witness.

On the merged tree, with the sink fix already in, switching the refusal off
costs **20 of the 56 classes** and 296 test methods (`53 OK / 1 FAIL / 2
TIMEOUT` becomes `34 OK / 21 FAIL / 1 TIMEOUT`; 6 failed methods becomes 302).
The families are the caching and reactive ones this cluster's sibling page
grouped under Spring's `"Post-processing of merged bean definition failed"`
wrapper. So the abort was the loud minority; the silent replay was the rest,
and it survived the sink fix.

The witness is a wrong answer rather than an abort:

| arm (one binary, one variable) | side effect ran | should be | `jit-callsite` sinks fired |
|---|---:|---:|---:|
| `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | **200 241** | 200 000 | **241** (`jit-callsite-a`) |
| default | 200 000 | 200 000 | 0 |

`vm/tests/jit_site_trap_never_duplicates_a_side_effect.rs` is that witness,
with an anti-vacuity arm that fails if the probe stops planting a trap.

## The refusal

`plant_uncommon_trap` refuses to plant when the trap would not be resumable,
and a refusal returns `false`, which its callers already turn into
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

## What the guard costs

Nothing measurable, and on this workload it is a small **win** — which is the
expected sign once the mechanism is clear. A planted indy trap is
*unconditional*: `x64::driver`'s own note says "a compiled 0xba site is an
UNCONDITIONAL trap … so any execution of this artifact that reaches it
deopts". An optimizing body whose live path runs into one pays a deopt on
every call and is strictly worse than the single-pass body it superseded;
declining it hands the method back to a tier that runs it.

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
produce this ordering. Measured before the sink fix was merged, so the OFF arm
there is aborting rather than resuming; it is reported for what it is, and the
claim it supports is only that the refusal does not cost throughput.

Refusal volume, for scale: the javac loop probe reports 122 `site TRAP
REFUSED` against 13 `site TRAP planted` in five iterations
(`CRATONVM_DBG_IR_COMPILES=1`). A refusal is `ir_build_bail`, which falls back
to the **single-pass backend**, not to the interpreter — so a refused method is
still compiled. `ir_trap_refusal_census()` reports these by cause beside
`ir_trap_census()`'s plants.

## Evidence for the compiler-side refusal alone

Measured on a binary built BEFORE the sink fix was merged, so this arm isolates
the refusal. A 98-line standalone reproducer (`JavacLoopProbe.java`: repeated
in-process javac through an in-memory `JavaFileManager`, the shape
`TestCompiler` uses) was validated on HotSpot first, then run four ways:

| arm | result |
|---|---|
| HotSpot JDK 25 | 20 iterations, 0 failures |
| CratonVM, `dev` @ `e90fa0274` | **20 of 20 failed**, first failure at iteration 0 |
| CratonVM, refusal only | 20 iterations, 0 failures |
| CratonVM, refusal only, `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | **20 of 20 failed** |

Real suite, `TestCompilerTests` alone under `run-suite.sh run`:

| | found | succ | fail |
|---|---:|---:|---:|
| before | 22 | 3 | 19 |
| after | 22 | 22 | 0 |

The whole 19-class cluster is covered by the sibling page's 56-class run.
`TestClassScannerTests` and `HttpServiceProxyRegistrationAotProcessorTests`,
listed in the original page as unconfirmed members, both pass (`7/7` and
`5/5`).

## What this did NOT fix

`org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests`
is in this cluster and now passes **14 of 14, matching HotSpot** — but it
takes 1067 s where HotSpot takes 10.7 s, so it still reads as `TIMEOUT` at the
suite's default 180 s per-class cap. That throughput gap is its own open page,
`../../../known-issues/spring/beanregistrations-verylarge-throughput-20260907.md`;
before this fix the class failed too early to expose it.
`test.context.aot.AotIntegrationTests` is the mild version of the same
reporting problem — `OK` in 334 s alone (4 found / 2 succ / 2 skip, matching
HotSpot), which fits the cap on a quiet host and not on a loaded one.

This is the "a deterministic failure hides the next one" shape: the fix did not
cause either slowness, it exposed them.

## One measurement note, because it cost a wrong conclusion

An `export CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` left in the session's
persistent SSH shell leaked into every later run launched from it, including
the ones labelled "guard ON". Both arms of a four-run A/B therefore read as the
OFF arm — the guard looked inert, and a known-issue page saying so was written
before the environment was checked. `env -i PATH=… HOME=…` in the runner script
is what makes an arm's environment a fact rather than an assumption; the same
script then reports `planted=` and `refused=` per arm, so an arm that did not
actually differ says so in its own output.

## Relationship to the already-fixed sibling bug

`aot-cglib-dynamicclassfileobject-illegalargumentexception-20260811-FIXED.md`
shares the architecture (`DynamicJavaFileManager` / in-memory compile) and the
original page asked whether this was a residual of that defect. It is not:
that one was the JIT resolving a callee by class name across loaders; this one
is a trap-resumability gap in a different backend and would have hit any
method with the same shape, in any workload. The H2 side of the same defect
(three application `toString()`/`wrap()` methods, no javac involved) is the
clearest evidence the two are unrelated — see
`../../fixed-bugs/precise-deoptimization-unavailable-cross-suite-crash-20260907-FIXED.md`.

## Reproducing (historical)

```bash
cd apps/spring-suite-runner
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-suite.sh run \
  --only 'TestCompilerTests$' --tag repro
# before: found=22 succ=3 fail=19; after: 22/22/0
```

## Related

- `../../fixed-bugs/deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md`
  — the sink fix, the other half of this closure.
- `../../../known-issues/jit/jit-bridge-sinks-re-run-a-side-effecting-body-20260907.md`
  — the silent half, witnessed here and closed for site traps; still open for
  traps from an IR deopt guard.
- `jit-mode-explains-most-of-todays-56-class-fail-cluster-FIXED-20260907.md`
  — the other 37 classes.
