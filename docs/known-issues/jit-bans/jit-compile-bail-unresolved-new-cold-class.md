# JIT coverage gap: a hot method never compiles if it contains `new <not-yet-loaded class>`

**Status:** OPEN (diagnosed, not fixed). Not a correctness bug — affected
methods stay interpreted, which is safe but can be very slow. Found on
2026-07-27 while re-testing the json-smart JIT ban
(`docs/internal/jsonsmart-parser-jit-retired-20260727.md`).

## Symptom

A method that is unambiguously hot never leaves the interpreter.
`CRATONVM_DBG=jit-method-stats` names them (`tier_fail_count=3` = the compiler
gave up permanently):

```
[cratonvm] JIT method stats: 67 distinct methods tracked, 66 ever invoked, 1581659 total invocations
           | still-interpreted=17 c1=12 c2=38 | c1_threshold=500 hot_but_stuck_in_interpreter=16
[cratonvm]  293940 queued=false tier_fail_count=3  net/minidev/json/parser/JSONParserBase.readMain(...)
[cratonvm]  245940 queued=false tier_fail_count=3  net/minidev/json/parser/JSONParserMemory.readString()V
[cratonvm]  230964 queued=false tier_fail_count=3  net/minidev/json/parser/JSONParserBase.checkControleChar()V
...
```

293,940 interpreted invocations of the workload's hottest method.

## Cause

Every one of those methods contains `new net/minidev/json/parser/ParseException`
on an error path. `resolve_jit_new_site` (`vm/src/runtime/interpreter.rs`)
resolves a `new`'s CP index with `find_class_by_name_for_class`, which only
sees **already-loaded** classes. When no parse ever fails, `ParseException` is
never loaded, so the resolver returns `None`, `try_compile_inner` bails the
WHOLE compile (`cp_new_resolver` / `new_resolve` site), and after
`MAX_TIER_FAIL_RETRIES` (3) attempts the method is never retried.

The bail is silent by design (it looks "transient"), which is what makes this
hard to spot. `CRATONVM_DBG_JITC=1` now names the resolver responsible:

```
[cratonvm-jitc] resolver-bail site=new_resolve net/minidev/json/parser/JSONParserBase.readMain(...)
[cratonvm-jitc] compile-bail   net/minidev/json/parser/JSONParserBase.readMain(...) backend_attempted=false
```

(That per-site naming was added with this investigation; before it, every
resolver miss produced the same anonymous `backend_attempted=false` line.)

The shape generalises far beyond json-smart: **any hot method whose only
un-taken branch does `throw new SomeException(...)`** is uncompilable until
something else in the process loads that exception class. Cold `new` of a
lazily-used helper class has the same effect.

## Why the obvious fixes are not obviously right

- **Resolve (load) the class at compile time.** Loading runs a user-defined
  `ClassLoader.loadClass` in the general case — arbitrary Java code from inside
  the JIT compile path, with the class-manager lock in play. That is a
  deadlock/reentrancy hazard (cf.
  `httpclient-hangs-are-classmanager-rwlock-deadlock...`), and it makes the VM
  load classes the program never would have.
- **Emit an uncommon trap at the unresolved `new`** (HotSpot's answer). The
  machinery exists (`DEOPT_REASON_UNREACHED_CODE`, reason 8, used for
  `invokedynamic`), but CratonVM's default deopt is a *re-run from method
  entry*, which double-executes any side effect committed before the trap —
  unsound for exactly the methods that motivate this (a parser that has already
  advanced its cursor). It would need the precise frame-deopt path
  (`CRATONVM_DEOPT_REAL`), which is not on by default.
- **Resolve at runtime instead of compile time.** Emit the existing slow-path
  allocation call with `(holder_class_id, cp_idx)` and add a
  `jit_new_object_cp` helper that resolves + initialises the class exactly like
  the interpreter's `0xbb` handler, then falls into `jit_new_object`'s body.
  This is sound (it is what the interpreter already does, on the same thread,
  at the same program point) and costs nothing on the hot path, but it touches
  the allocation codegen, adds a `JitRuntimeHelpers` field (~10 test files
  construct that struct exhaustively) and deserves suite-wide validation before
  it lands by default. **This is the recommended fix.**

## Reproduction

`docs/known-issues/repros/jsonsmart/JsonSmartProbe.java` (unmodified) against
`json-smart-2.6.0.jar`, with `CRATONVM_DBG=jit-method-stats`. Then run
`JsonSmartProbeWarmed.java`, which is identical except that it first drives five
deliberately malformed documents through the parser: that single change loads
`ParseException`, and the same 14 methods compile immediately
(`hot_but_stuck_in_interpreter` drops from 16 to 2, and total interpreted
invocations from 1,581,659 to 81,340).
