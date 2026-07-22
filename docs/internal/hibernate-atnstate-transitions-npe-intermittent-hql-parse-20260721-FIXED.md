# Hibernate HQL ANTLR `ATNState.transitions` intermittent NPE

**Status:** FIXED — 2026-07-22

## Root cause

Hibernate's unshaded ANTLR parser uses CratonVM's native
`ParserATNSimulator` closure/reach-set implementation.  It retained raw
`ObjectRef` values for the simulator, configs, ATN states, transitions, and
DFA graph across Java calls and allocations.  A moving collection could then
leave a resumed native closure walk indexing a stale `ATNState.transitions`
array, producing the intermittent `NullPointerException` observed from
`ParserATNSimulator.computeTargetState`.

The same run exposed a related cleanup residual: the real
`ThreadPoolExecutor` shutdown bridge retained its executor and `ctl` only as
raw references while calling Java lifecycle hooks.  JUnit's timeout executor
could consequently reach `tryTerminate()` with a stale receiver and report
`this.ctl` as null while closing the extension context.

## Fix

- Added balanced native-root frames around the native ANTLR closure graph and
  Java delegation boundaries, with post-allocation re-reads of `state`,
  transition, config, and simulator references.
- Re-read each reach-set `ATNState` from its rooted `ATNConfig` before
  indexing the next transition.
- Rooted the real executor shutdown receiver and `ctl` across its AtomicInteger,
  shutdown-hook, and `tryTerminate` calls.

The earlier deterministic `inContext = depth == 0` ANTLR semantic correction
is preserved unchanged.

## Validation

Using `C:\craton\CratonVM\apps\hibernate-orm` and
`C:\craton\CratonVM\apps\hib-suite-runner`, JDK 25.0.3, and unique binary
`C:\craton\targets\atnstate-transitions-20260721-019f874b-release4\release\cratonvm.exe`:

- native registry test:
  `cargo test -p cratonvm-native-builtins antlr_prediction_context_intrinsics_are_registered --lib -- --nocapture`
  — pass.
- JIT, two fresh runs of the original two-class fixture — both clean:
  `HQLTypeTest` 2/2 and `JsonFunctionTests` 14/14 (20 intentionally skipped),
  no ATN NPE and no JUnit extension-close failure.
- `--nojit`, same fixture — clean with the same counts.
- Direct nested-HQL parser probe passes in both modes for one iteration.

The generic `gen_heap::read_slot: corrupt Value cell` diagnostic occurred 32
times in both the pre-final and fixed JIT `HQLTypeTest` runs, while the
pre-final binary still produced the JUnit close failure.  It is therefore
pre-existing, non-fatal diagnostic noise rather than a residual of this
ATN-transition repair.

## Scope

This closes the original `HQLTypeTest` / `JsonFunctionTests` intermittent
parse NPE and the directly observed JUnit timeout-executor cleanup residual.
