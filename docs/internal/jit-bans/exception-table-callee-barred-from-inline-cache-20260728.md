# A callee that declares an exception table is barred from the inline-cache fast path

**Status:** FIXED 2026-07-30. The direct-entry MIC, PIC and static paths now service the callee sentinel and resume its own handler without replaying side effects.

## The measurement

`LazyIsolate` d8/d9 — two methods with **identical bodies**, one wrapped in a
`try`/`catch` whose handler reads only parameters, one not. One binary, one run,
300k iterations, single thread:

| variant | ns/op | compiled code size |
|---|---|---|
| d8 `getParamOnlyHandler` — has a `try`/`catch` | ~4200 | `len=1954` |
| d9 `getParamOnlyNoTry` — identical, no `try` | ~610 | `len=1949` |

**~7x apart on machine code that is five bytes different.** Whatever costs the
7x, it is not code quality — it cannot be, the two bodies compile to the same
thing. It is per-call overhead paid by the *caller*.

## Root cause

`vm/src/jit/helpers.rs` refuses to publish a compiled callee into the
monomorphic/polymorphic inline cache when that callee declares an exception
table — both at the cache-hit republication site (~8147) and at the initial
population site (~8304):

```rust
if cacheable_receiver
    && !mic_callee_has_exception_table(vm, receiver_class_id, info)
    && !compiled_entry_has_indy_trap(...)
```

The gate's own comment states the consequence plainly: *"Leaving the cache empty
keeps every dispatch on the helper's `invoke_or_native` path below."* So every
call to such a callee pays the full dispatch helper instead of the inline
cascade — the same comment prices that hit at *"5 cycles slot-0 hit vs the full
helper call"*.

The statically-bound sibling gate lives in the `callee_compiler` closure in
`interpreter.rs`.

## Why the gate exists (do not just delete it)

A JIT frame cannot dispatch to its own handler. When a compiled callee with an
exception table throws, correctness depends on the runtime intercepting the
`i64::MIN` sentinel and re-running the callee in the interpreter
(`bail_to_interpreter` in `route_implicit_exception_through_callee`), which is
the only thing that consults the exception table. **Only the dispatch helper can
do that.** A machine-code `CALL` to a cached entry bypasses it, and the
exception escapes the callee's own `catch` — the concrete victim named in the
gate's comment is Tomcat's `HttpParser.isNotRequestTargetRelaxed`, whose
`catch (AIOOBE)` was skipped.

So this is a real correctness/throughput trade, not an oversight. Removing it
requires giving compiled code a way to enter its own handler (or a cheap
caller-side pending-exception check that can still route through the callee's
table) — not simply widening the cache.

## Scale

`try`/`catch` is ordinary Java. This tax applies to **every call to every method
that declares a handler**, in every workload, regardless of whether the handler
ever fires. On the evidence above that is roughly 7x per call for a small
callee.

## What this corrects

The 2026-07-27 write-up in
`docs/known-issues/tomcat/23-charsetcache-pathological-slowdown.md` attributed
this same d8/d9 gap to the optimizing tier refusing exception-table methods
(`cached.exception_table.is_empty()` in `try_compile_inner`). That exclusion was
real and has now been fixed — but it is **not** what the d8/d9 pair measured:
`getParamOnlyHandler` compiles to `len=1954` with the fix on *and* off, i.e. it
never qualified for the optimizing tier in the first place (it is held off by
`ir_compatible`, not by the exception table). The correlation was right; the
attribution was wrong. See that document's correction section.
