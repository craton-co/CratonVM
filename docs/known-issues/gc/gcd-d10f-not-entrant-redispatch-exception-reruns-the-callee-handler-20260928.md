# An exception from a not-entrant re-dispatch is offered to the callee's handlers a second time (suspected)

> **STATUS (2026-09-29, gce ve2): OPEN -- only 2 counted runs (Gen 1, ZGC 1) of 3 per collector; the control never re-dispatched. Rerun f_<gc>_notentrant with 10 reps.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e2/f): REPRODUCED on the base; the probe is now
> decisive.** The rewritten `tools/probes/GceE1fNotEntrantEscapeProbe.java`
> calls `m` through a monomorphic interface call (the single-pass caller's
> inline cache names the stale compiled `m`), redefines `Callee` with one
> constant changed (so `mark=` proves the NEW `m` ran), and exits 2 with
> `no agent PROBE-SKIP` when run without `-javaagent`. On the base
> (`adb9178bc`, Windows, Generational) with `CRATONVM_DBG_JITC=1
> CRATONVM_DBG_DEOPT=1` it reached the stub (`not-entrant entry:
> re-dispatching GceE1fNotEntrantEscapeProbe$Callee.m`) in 5 of 8 runs and
> printed `redefined=true mark=2000003 logs=1 caught=1 PROBE-FAIL` in every
> one of them -- the suspected double handler run, confirmed. Without
> `CRATONVM_DBG_JITC=1` the stub was reached in 0 of 12 runs (the timing
> leaves the IR caller running, whose call goes through the Rust door).
> HotSpot 25.0.3 (`-XX:+UseSerialGC`, and `-Xint`): `redefined=true
> mark=2000003 logs=0 caught=1 PROBE-OK`.
>
> **Rows (each collector; build once):**
> ```
> javac -d out tools/probes/GceE1fNotEntrantEscapeProbe.java
> jar cfm out/GceE1fNotEntrantEscapeProbe.jar tools/probes/GceE1fNotEntrantEscapeProbe.mf -C out .
> A="-Xmx64m -javaagent:out/GceE1fNotEntrantEscapeProbe.jar -cp out/GceE1fNotEntrantEscapeProbe.jar"
> p ${gc}_notentrant_$r 300 "CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1" "$X $A" GceE1fNotEntrantEscapeProbe
> p ${gc}_notentrant_off_$r 300 "CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1 CRATONVM_JIT_NOT_ENTRANT_ESCAPE=0" "$X $A" GceE1fNotEntrantEscapeProbe
> ```
> Run r = 1..6. Retire when, over the default rows, every run whose stderr
> has `>= 1` `re-dispatching GceE1fNotEntrantEscapeProbe` line prints
> HotSpot's line, with at least 3 such runs per collector; the `_off` rows
> should print `logs=1 ... PROBE-FAIL` whenever they re-dispatch (the
> control). A run with no re-dispatch line judges only `mark` and `caught`.

> **STATUS (2026-09-29, gce e1/x): KEEP -- the probe run was VACUOUS.** The e1 unit tests pass (Windows suite). The ve1 rows `*_notentrant_1..3` ran `GceE1fNotEntrantEscapeProbe` WITHOUT `-javaagent`, so the probe printed `no agent` (as HotSpot does without the agent) 9/9: nothing was redefined. **Remaining:** the agent run on each collector (`CRATONVM_DBG_DEOPT=1 cratonvm <gc> -javaagent:probe.jar -cp probe.jar GceE1fNotEntrantEscapeProbe`, 3x), expected `redefined=true logs=0 caught=1 PROBE-OK`, noting whether the `not-entrant entry: re-dispatching` line appears (lane f: the probe is a guard, not a reproducer).

> **STATUS (2026-09-29, gce e1/f): FIXED IN CODE, awaiting the probe --
> CONFIRMED by reading, on TWO doors, not one; default on.**
>
> - **Confirmed.** `jit_not_entrant_entry_body` answers a throw with
>   `handle_jit_dispatch_error` -> `set_jit_pending_exception` (`athrow_bci =
>   -1`) and the sentinel. Both doors that can receive it then treat the
>   throwable as one that has not yet met the callee's table:
>   `handle_compiled_callee_deopt_sentinel` (the compiled call site's
>   service, and the MIC / lambda doors through it) runs
>   `try_run_callee_handler` with `throw_pc = usize::MAX`, where
>   `find_jit_exception_handler`'s pc-unknown search matches a typed row on
>   class alone; and `route_implicit_exc_through_callee` (the dispatch
>   helper's door, reached when a stale compiled entry it called was patched)
>   does the same and, on a pc-unknown miss, re-runs the callee FROM ENTRY
>   (the KCFULL-13 re-run), which replays all of it. No precise frame exists
>   to correct the pc (the interpreter ran the method).
> - **Fix (`vm/src/jit/helpers.rs`):** a per-thread escape record,
>   `JitSignals::redispatch_escape` (the method's name+descriptor hash; `0` =
>   none). `jit_not_entrant_entry_body` sets it when it returns the sentinel
>   with a throwable pending (`note_redispatch_escape`, both arms); every
>   pending-exception setter and `take_all_jit_signals` clear it; the two doors
>   take it FIRST (`take_redispatch_escape_for`) and, when it names the
>   callee of this very site, skip the callee-table search (and the KCFULL-13
>   re-run) and propagate, as a `NotCaught` miss does. A record naming
>   another method is dropped unused, so a stale one cannot skip a real
>   handler one level up. Kill switch `CRATONVM_JIT_NOT_ENTRANT_ESCAPE=0`.
> - **Not covered (by reading, outside this lane's files):** the
>   interpreter's `RawJitBodyOutcome::Throw` arm (`runtime/interpreter.rs`)
>   routes a body's throw against that body's own table with an unknown pc
>   too; a not-entrant stub reached THERE would need the same record. The
>   interpreter enters bodies it looked up in the cache, which a redefinition
>   has already replaced, so the stub is not expected there.
> - **Probe:** `tools/probes/GceE1fNotEntrantEscapeProbe.java` (agent +
>   identity redefinition; SETUP in its header). HotSpot 25.0.3
>   `-XX:+UseSerialGC` and `-Xint`: `redefined=true logs=0 caught=1
>   PROBE-OK`, rc 0. **On the base binary it did not reach the re-dispatch**
>   (0 `not-entrant entry: re-dispatching GceE1fNotEntrantEscapeProbe$Callee.m`
>   lines under `CRATONVM_DBG_DEOPT=1` at warm-ups 2e4, 2e5, 1e6, 3e6): the
>   caller's baked target was an earlier body of `m` than the two the
>   redefinition patched (`CRATONVM_DBG_JITC=1`: `full-compile ...m` bound at
>   `0x...410000`, `not-entrant: ...m entry=0x...430000/0x...470000`). So
>   the probe is a regression guard, not yet a reproducer; a PASS without the
>   re-dispatch line proves nothing.
> - **Tests:** `cargo test -j 5 -p cratonvm-vm --lib gce_e1f_a_redispatch_escape_speaks_once_for_its_own_callee`.
> - **Run (each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`, `-XX:+UseZGC`):**
>   build the jar as the probe says, then
>   `CRATONVM_DBG_DEOPT=1 cratonvm <gc> -javaagent:probe.jar -cp probe.jar GceE1fNotEntrantEscapeProbe`
>   x3: `redefined=true logs=0 caught=1 PROBE-OK`, rc 0; report whether the
>   re-dispatch line appeared. The redefinition battery
>   (`RedefineCompiledOldConstantsProbe` and the `tools/probes/interp/L2`,
>   `L3` redefinition probes) must keep its recorded outputs.

> **STATUS (2026-09-28, gcd d10/f, lane frames10): OPEN, SUSPECTED, filed
> from reading during the adversarial review of the callee-deopt service; not
> reproduced (no build in this lane, and a deterministic probe needs a
> retransforming agent).** A wrong result if it reproduces, so the fix may
> land default on. Owner: a later wave of the JIT frame lane
> (`vm/src/jit/helpers.rs`). Not a collector defect: every collector runs the
> same path.

*Filed 2026-09-28 by gcd wave d10, lane f (frames10).*

## What is wrong, by reading

A class redefinition patches every stale compiled body's entry with a jump
to its not-entrant stub. A compiled caller whose baked direct `CALL` still
names that entry lands in `vm/src/jit/helpers.rs` `jit_not_entrant_entry_body`,
which runs the call through the interpreter (`invoke_static_shared_on_class`
or `bail_to_interpreter`) on the class's CURRENT bytecode. The interpreter
runs the whole method, its exception table included. An exception that
escapes it comes back through `handle_jit_dispatch_error` ->
`set_jit_pending_exception`, which resets `JIT_SIGNALS.athrow_bci` to `-1`
("unknown"), and the stub returns the `i64::MIN` sentinel to the call site.

The call site's callee-deopt check then runs the service
(`jit_service_callee_deopt_body` -> `handle_compiled_callee_deopt_sentinel`)
as for a COMPILED callee that threw: `callee_has_exception_table` (by name:
the current method's table) and, when it is non-empty,
`try_run_callee_handler` with `throw_pc = usize::MAX`. The pc-unknown search
in `run_jit_callee_handler` matches a TYPED handler by exception class alone,
ignoring its protected range (the behaviour `run_jit_callee_handler`'s
2026-09-22 note documents), and a catch-all spanning the whole method. So a
handler the interpreter already considered -- and the exception left, either
because the handler did not cover the throw site or because the handler
itself rethrew -- is run a second time, with params-only locals when the
method does not need precise ones:

```java
static void m(Path p) throws IOException {
    try { a(); } catch (IOException e) { log(e); }   // covers a() only
    Files.delete(p);                                 // throws IOException
}
```

After a retransformation of `m`'s class, the first call from a compiled
caller with a baked `CALL`: the interpreter runs `m`, `Files.delete` throws,
the exception leaves `m`; the service matches the `IOException` row, runs
`log(e)` and then `Files.delete(p)` again from the handler's fall-through.
HotSpot propagates the first exception.

- **Reach:** any class an agent retransforms (APM agents retransform at
  startup and on demand), any method of it with an exception table, called
  from a compiled caller with a baked direct `CALL` (the not-entrant stub
  forwards later calls to the new body, `NotEntrantRecord::forward_to`, so
  mostly the first call(s) after the redefinition, and every call while an
  interpreter-only agent event keeps forwarding off).
- **Severity:** wrong result (handler side effects and the rest of the
  method re-run), rare by the population it needs.

## Proposed fix

Mark the throwable as having already left the callee. Either:

1. a `JitSignals` bit (`callee_ran_interpreted`), set by
   `jit_not_entrant_entry_body` beside the pending exception, drained by
   `take_all_jit_signals` and by EVERY single-signal drain
   (`take_jit_pending_exception` and the entry hygiene in
   `call_compiled_entry_under_owner`: a stale bit would skip a real handler),
   and read by `handle_compiled_callee_deopt_sentinel` as
   `has_handler = false` (propagate, as the `NotCaught` miss does); or
2. stamp `athrow_bci` with a pc no exception row can cover (`65536`, past
   the JVM's code-length limit), so `run_jit_callee_handler` answers
   `NotCaught` from a KNOWN pc. Smaller, but every `athrow_bci` consumer
   (77 references across `helpers.rs`, `exception_dispatch.rs`,
   `jit_bridge.rs`, `interpreter.rs`) must be audited for an index into the
   code first.

## How to verify

A probe with a `java.lang.instrument` agent (`Agent-Class` +
`Can-Retransform-Classes` manifest) that retransforms `m`'s class (an identity
transform suffices: the redefinition is what patches the entry) after a
caller of `m` has been compiled, then calls the caller once with a path whose
deletion fails (and an `a()` that does not throw). HotSpot: `log` never
runs, the `IOException` propagates to the caller, counter `logs=0`. This VM, if the
suspicion holds: `logs=1`. Run with each of `-XX:+UseGenerationalGC`,
`-XX:+UseG1GC`, `-XX:+UseZGC`, and `CRATONVM_DBG_DEOPT=1` shows the
`not-entrant entry: re-dispatching` line followed by a handler run for the
same method.
