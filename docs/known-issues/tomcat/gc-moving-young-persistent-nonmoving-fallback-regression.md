# Throughput-wall recurrence #3: moving-young GC now falls back to the non-moving sweep persistently — `TestHostConfigAutomaticDeployment*`, `TestNonBlockingAPI` hang standalone

| | |
|---|---|
| **Status** | OPEN — third recurrence of a previously-closed issue |
| **Severity** | high — breaks a "passes standalone in ~30s" guarantee the prior closures relied on |
| **Discovered** | 2026-08-06, complete 651-class Tomcat suite rerun after merging `dev` (~1044 commits) |
| **Prior history** | [`docs/internal/tomcat/04-embedded-server-throughput-wall-CLOSED.md`](../../internal/tomcat/04-embedded-server-throughput-wall-CLOSED.md) (closed 2026-07-27), [`docs/internal/tomcat/29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md`](../../internal/tomcat/29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md) (a second recurrence, also closed) |

## Symptom

Doc 04's closing evidence measured `TestHostConfigAutomaticDeploymentAddition`
running **alone** (no shard contention) at **32.2s**, and used that number to
argue the family's HANGs under sharded load were a scheduling/contention
artifact, not a VM defect. That guarantee no longer holds:

- The complete 1-shard suite run (2026-08-05/06, no other CratonVM/java
  process running on this host at the time — confirmed via `Get-Process`)
  produced **HANG at the full 1500s timeout** for all 9
  `TestHostConfigAutomaticDeployment*` classes it reached, plus
  `TestNonBlockingAPI` (previously a reliable PASS at 600–780s in every prior
  run this session, 4-shard or otherwise).
- Standalone re-verification of `TestHostConfigAutomaticDeploymentAddition`
  (single process, this session, host confirmed idle of other CratonVM/java
  processes): after **300+ seconds** the class had only reached its **4th**
  test method (`grep -c "Starting test case"` on the live log), versus doc
  04's ~32s for the *entire* class. The process was making genuine forward
  progress (not deadlocked) — just roughly 40-80x slower than the closed
  baseline.

## The new evidence: persistent moving-young → non-moving fallback

Every few seconds during the standalone repro, the log emits:

```
WARN cratonvm_gc::gc_quiescence: [moving-young] fallback #1: reason=innermost-rbp-belongs-to-unguarded-callee
  — a live JIT frame could not prove a complete rewritable root map, so this
  young collection runs the NON-MOVING sweep (no compaction, free-list
  allocation). Persistent fallbacks mean the young generation is not
  actually a copying collector.
WARN cratonvm_gc::gc_quiescence: [moving-young] fallback #2: reason=compiled-frame-oop-not-published ...
WARN cratonvm_gc::gc_quiescence: [moving-young] fallback #3: reason=compiled-frame-oop-not-published ...
```

The message is explicit about the consequence: a free-list-allocating,
non-compacting young generation is dramatically slower under sustained
allocation than the copying collector it's supposed to be, and this test
family's HostConfig deploy/redeploy cycle is exactly the kind of
allocation-heavy workload that would expose that. The fallback keeps firing
(#1, #2, #3, ... — not a one-time warmup event), meaning the *"innermost-rbp
belongs to an unguarded callee"* / *"compiled-frame-oop-not-published"*
conditions are hit routinely for this workload's JIT'd frames, not rarely.

This same fallback spam was also observed leading into every occurrence of
the ECJ `OperandStack` corruption, and a shared root cause was proposed there.
**That half is now settled and it is NOT a shared root cause** — see
[ecj-operandstack-corruption-jsp-compilation-500s-FIXED.md](../../internal/fixed-suite-bugs/tomcat/ecj-operandstack-corruption-jsp-compilation-500s-FIXED.md).
The ECJ symptom was a codegen slot-accounting defect: the x64 single-pass
backend pushed a raw JIT-to-JIT call's return value from a spill cursor the
service-argument reservation had already moved, so the result landed `n`
operand-stack slots too deep and every branch target after the call read a
different slot than the call wrote.

The correlation was real, but the direction is the other way round: both come
from the same feature. `innermost-rbp-belongs-to-unguarded-callee` is the GC
*declining* to trust a precise root map when the innermost frame was entered by
a raw JIT-to-JIT call it cannot decode — the fail-closed safety net working —
and that same raw-call feature is what carried the codegen defect. The fixed
binary still emits moving-young fallbacks on the ECJ classes and returns `OK`,
so the fallback does not corrupt anything by itself. What that leaves for THIS
doc is unchanged: the fallbacks are a throughput problem, and the open question
is still whether the two named conditions are a regression in the frame-safety
proofs or new categories introduced by the recent JIT/interpreter work.

## Not yet established

- Whether this is a genuine regression in the moving-young GC's frame-safety
  proofs, or whether the two conditions it names (`innermost-rbp-belongs-to
  -unguarded-callee`, `compiled-frame-oop-not-published`) are new categories
  introduced by the recent JIT/interpreter work (the `interpreter.rs` split
  into `vm/src/runtime/interpreter/{dispatch_virtual,gc_and_alloc,jit_bridge,
  ...}.rs` landed in this same merge).
- Whether `CRATONVM_MOVING_YOUNG=0` or forcing the non-moving path
  deliberately changes the timing (would confirm this diagnosis without
  further guessing).
- Full standalone timing for the other 8 HostConfig classes and
  `TestNonBlockingAPI` (only `Addition` was directly re-verified standalone
  this round; the others are inferred from the identical HANG-at-ceiling
  symptom in the full-suite run).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.catalina.startup.TestHostConfigAutomaticDeploymentAddition
```
Watch stderr for `[moving-young] fallback #N` lines and count `Starting test
case` occurrences in stdout to gauge real progress versus a true deadlock.
