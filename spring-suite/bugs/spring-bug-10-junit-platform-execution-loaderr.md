# spring-bug-10: JUnit-platform execution failures on (AspectJ-woven) AOP test classes

| | |
|---|---|
| **Category** | VM-CORRECTNESS / dispatch (JUnit platform internals) |
| **Module** | spring-aop, spring-beans (AspectJ-woven + a few others) |
| **CratonVM** | LOADERR — exception thrown from `launcher.execute()` before any RESULT |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | c5644da4 (pre-fixes) |
| **Status** | OPEN (documented) |
| **Suggested owner** | handoff (mixed causes; some overlap with bug-03 dispatch) |

## Symptom
~18 classes (mostly `spring-aop` AspectJ tests, compiled with `compileAspectj`) throw from inside
the JUnit Platform engine during discovery/execution, so KRun reports `LOADERR`. Distinct causes:

| count | error |
|------:|-------|
| 16 | `ClassCastException: java/lang/Object cannot be cast to org/junit/platform/engine/TestExecutionResult$Status` |
| 10 | `NullPointerException: Cannot invoke length on null` |
| 7 | `AbstractMethodError: org/junit/platform/engine/TestEngine.getId()Ljava/lang/String; has no Code attribute` |
| 5 | `NullPointerException: Cannot invoke executionStarted on null` |
| 1 | `AbstractMethodError: java/util/function/Predicate.test(...)Z has no Code attribute` |
| few | `NoClassDefFoundError` (MutinyRegistrar, TestCompiler, ExceptionUtils) |

## ★ VALIDATED this session — GC race confirmed; real fix = fix the moving/remap SIGSEGV (deferred B-K Stage B/C)
Reproduced + validated directly (dev `334fe5e7`):
- **Reproduced** the race: a 14-class `org.springframework.aop.aspectj.*` batch (these classes live in
  **spring-context**'s test output, not spring-aop — my first repro ClassNotFound'd on the wrong CP)
  produced **101 "Stale pointer detected … all-zero header → falling back to CP class java/util/List"**
  warnings under default flags. So the GC stale-pointer race is real and reproducible.
- **`CRATONVM_SHADOW_STACK=1` → 101 stale warnings drop to 0** (the shadow-stack **marking** keeps the
  register-invisible oops alive) **BUT the run then SIGSEGVs (rc=139)** — the **moving/remap** half
  (Cheney-while-in-JIT + `shadow_stack.remap`) that the same flag enables is what crashes.
- **A "marking-only default" (the proposed quick fix) is NOT safe** — I implemented + then reverted it:
  the marking root publish (`vm/src/memory/roots.rs:216`) emits shadow oops as **movable** (the B-K
  design, to avoid OOM), relying on the non-moving sweep's **selective promotion to EVACUATE** them,
  which needs the **remap**. With remap gated off (marking-only), a *promoted* shadow object's slot
  goes **stale** → the race returns for promoted objects. Publishing them **pinned** instead avoids
  that but **re-introduces the bt18 small-heap OOM** the B-K change specifically fixed ("pinning every
  register-invisible operand-stack oop OOMs at small heap").
- **Therefore the real, load-bearing fix is to make the FULL shadow mode (movable + remap + moving)
  not SIGSEGV** — then the existing movable+remap path resolves the race with no OOM. That SIGSEGV is
  the deferred **B-K Stage B/C** GC work (fix the Cheney-while-in-JIT / `shadow_stack.remap` crash),
  needing **uncontended-machine bt16/bt18 validation**. Gate split alone does not fix it.
- **Per-run mitigation:** none clean (full flag crashes; marking-only is unsafe). The conservative
  scan already pins stack-resident oops; only the register-invisible ones leak — so the bug is
  bounded to heavy-multithreaded register-oop-across-call shapes.

## (corrected) ROOT CAUSE — GC root-undercount race, NOT dispatch/weaving
All sub-causes are **one bug**: a **young-gen GC stale-pointer / cross-thread root-undercount race**.
Under heavy multithreaded JUnit execution, a live platform-engine/listener/enum object is reclaimed
(header zeroed) or left stale because a root referencing it (operand-stack / register-resident on a
parked worker thread) is invisible to the root scan. Evidence in `results-cv/*/crashes.log`: every
LOADERR is immediately preceded by `WARN …interpreter: Stale pointer detected in invokevirtual
receiver (… all-zero header) — falling back to CP class …NodeTestTask/TestEngine/Predicate/…`.

The surface error just depends on which object was zeroed:
- **`TestEngine.getId()` / `Predicate.test()` "no Code"** — stale receiver (`class_id_of`→Object/0);
  the stale-pointer fallback at `interpreter.rs:10766` dispatches on the **CP method-ref class**,
  which for an `invokeinterface` IS the abstract interface → no Code → AbstractMethodError. The
  S111r10 receiver-walk rescue **cannot** help (the receiver is dead, not merely interface-stamped —
  contrast bug-03). So this is **not** the bug-03 pattern.
- **`Object → TestExecutionResult$Status` CCE** — the `Status` enum constant flowing through JUnit's
  `Preconditions.notNull` (plain bytecode, returns its arg) is zeroed; `checkcast`
  (`interpreter.rs:8712`) then fails. (Corroborated by a flood of `CompletableFuture` CAS
  `NoSuchMethodError` on zeroed Object receivers nearby.)
- **NPEs** (`executionStarted`/`length` on null) — a zeroed listener/`UniqueId` field reads null.

**Mechanism:** per-thread root snapshots (`interpreter.rs:1322-1478`) + the default non-moving young
sweep that still selectively promotes (`gc/src/gen_heap.rs:2045-2092`); a GC firing while a JUnit
worker is parked with a stale/incomplete `root_snapshot` misses the worker's register/stack roots —
the same register-invisibility gap `CRATONVM_SHADOW_STACK` (default-OFF,
`vm/src/jit/conservative_roots.rs:189-204`) was built for. **Load-dependent:** light single-threaded
spring-aop classes produce ZERO stale warnings; only the long-lived multithreaded full-suite run
triggers it. **NOT AspectJ weaving** (affected classes verified non-woven).

### Proposed fix (high-leverage — recovers ~all genuine LOADERRs + likely some CRASH/FAIL)
Close the cross-thread root undercount: make the precise **shadow-stack roots default-ON for the
marking half** (the kafka B-K change already wires shadow oops as *pinned* roots into the non-moving
sweep — gate the marking fold-in `interpreter.rs:1472-1478` on always-on rather than
`shadow_stack_enabled()`; keeps the non-moving sweep, only over-pins). **High risk / needs uncontended-machine
validation** (overlaps the deferred B-K Stage B/C flip in memory `precise-jit-maps-bk-status`).
Validate first by re-running the LOADERR/CRASH classes with `CRATONVM_SHADOW_STACK=1`.
Diagnostic: `CRATONVM_DBG_STALE_RECV=1` (`interpreter.rs:10710`) dumps the frame/locals holding the
stale address.

### Drop from the CV-bug tally (environmental, NOT CratonVM bugs)
- **~14 TestNG-engine LOADERRs** — `JUnitException: TestEngine with ID 'testng' failed to discover`
  (`*TestNG*` classes, `test.context.jdbc.*`): need the TestNG engine; separate env/engine issue.
- `NoClassDefFoundError: …MutinyRegistrar / TestCompiler / ExceptionUtils` — optional deps absent.

## (superseded) earlier analysis
- **`TestEngine.getId()` / `Predicate.test()` "has no Code attribute"** — the SAME synthetic-object-
  stamped-with-interface-class dispatch bug as [[spring-bug-03]] (a synthetic `TestEngine` / lambda
  `Predicate` resolves to the abstract interface method). Likely fixable the same way (ensure the
  concrete impl/native is dispatched), and would recover the 7+1 classes.
- **`ClassCastException … TestExecutionResult$Status`** (16) — something returns a bare `Object`
  where JUnit expects the `Status` enum; an enum/result object isn't constructed with the right
  class. Needs its own trace.
- **NPEs on `executionStarted`/`length`** — a JUnit listener or reflectively-obtained value is null
  under CratonVM where HotSpot has it.

These manifest specifically on **AspectJ-woven** classes (synthetic woven methods/fields confuse
CratonVM's reflection during JUnit discovery), plus a few with missing optional deps
(`MutinyRegistrar` = reactor-mutiny not on cp → environmental, not a CV bug).

## Reproduce
```bash
CP="$H;$(tr -d '\r' < .../spring-aop/build/cratonvm-testcp.txt)"
KRUN_STACK=1 "$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.aop.aspectj.autoproxy.AnnotationPointcutTests
"$JDK\bin\java.exe" -cp "$CP" KRun ...AnnotationPointcutTests   # OK
```

## Notes
Mixed root causes; the `getId()`/`Predicate.test()` "no Code" subset overlaps [[spring-bug-03]]'s
fix pattern. The `NoClassDefFoundError: …MutinyRegistrar` cases are environmental (optional dep
absent) and should be dropped from the CV-bug tally. Good handoff once the dispatch subset is split
out.
