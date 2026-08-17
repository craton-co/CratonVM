# `TestUpgrade`'s `initCause` cascade — a subclass field that shadowed `Throwable.cause`

## Status
**FIXED 2026-08-14** — `fix/initcause-wrongcreds-20260814`.

**Root cause: CratonVM's `Throwable` natives WROTE `cause` at the slot
`java/lang/Throwable` declares and READ it at whatever slot the receiver's own
class resolves the name `cause` to.** Those are the same slot for almost every
throwable in existence, and a different slot for
`org.h2.jdbc.JdbcSQLException`, which declares its own `private final Throwable
cause` and assigns it on the line before it calls `initCause(cause)`.

So `initCause` read H2's field, found the value H2's constructor had just put
there, concluded the cause was already set, and refused the **first and only**
call on that receiver with `IllegalStateException: Can't overwrite cause with
a null`. Every `JdbcSQLException` H2 built failed to construct; each refusal
was caught and re-wrapped by the next JDBC layer, which produced the five-deep
nesting the original report opens with.

The `initCause` state machine that the original report suspected
(`180ceeb8e`, 2026-08-11) is correct and is **not** the defect. It is the
change that made the pre-existing slot mismatch *observable*: before it,
`initCause` was an unconditional setter, so reading the wrong slot cost
nothing because nothing acted on what it read.

## The measurement that named it

`CRATONVM_DBG_CAUSE=1` on the failing run, with a temporary dump of the
receiver's raw `cause` field beside the resolved index:

```
CAUSE_DBG_INIT this=org/h2/jdbc/JdbcSQLException hash=330
  raw=Object(None) declares=true idx=Some(12) nfields=17 already_set=true
  arg=Object(None)
CAUSE_DBG_INIT this=org/h2/jdbc/JdbcSQLException hash=368
  raw=Object(Some(ObjectRef { ptr: 0x20042c2db08 })) ... already_set=true
  arg=Object(Some(ObjectRef { ptr: 0x20042c2db08 }))
```

`raw` is the argument, pointer for pointer. The read was not returning
`Throwable.cause` at all — it was returning the field H2's constructor had
assigned one line earlier. `idx=Some(12)` of `nfields=17` is H2's declaration;
`java/lang/Throwable`'s `cause` is far below it.

## The fix

`shadowed_throwable_slot` (`native-builtins/src/lang_misc.rs`) resolves the
slot `java/lang/Throwable` itself declares and returns it **only** when the
receiver's own class declares a different field of the same name — the
shadowing case. `throwable_field_get` / `throwable_field_set` route every
name-keyed access to a `Throwable`-declared field (`cause`, `detailMessage`,
`suppressedExceptions`, `backtrace`, `depth`, `stackTrace`) through it, in
`lang_misc.rs`, `lib.rs` and `logmanager.rs`.

A receiver that merely *inherits* the field takes the identical path it took
before, so nothing outside the shadowing case changes. That is deliberate:
the read side of this pair carries several earlier fixes (the synthetic-stub
slot fallback, the `Object(None)` ambiguity gate) which are still load-bearing.

Javac resolves `getfield`/`putfield` against the class named in the constant
pool, which is why real `Throwable.initCause` bytecode is immune to this and
why the defect only exists where a native addresses the field by name on the
receiver.

## Verification

**The class.** `org.h2.test.unit.TestUpgrade`, which the report opens with:

| arm | before | after |
|---|---|---|
| CratonVM `--nojit` | FAIL (~0.1 s, deterministic) | **PASS** |
| CratonVM JIT | FAIL | **PASS** |
| HotSpot JDK 25 | PASS | PASS |

**The contract, against HotSpot.** Eight observables were added to
`probes/ShadowDifferentialProbe.java`'s `throwableSurface` section, covering a
subclass that shadows `cause` and one that shadows `detailMessage`. They
include the cases a half-fix passes by accident — that the subclass's own
field is still its own after `initCause`, and that a receiver whose cause
really *is* set still refuses a second call.

```
                                        HotSpot            before             after
shadowedCauseInitCauseSucceeds          no-throw           IllegalState…      no-throw
shadowedCauseGetCauseAfterInit          …IllegalState:real …IllegalArg:own    …IllegalState:real
shadowedCauseInitCauseNull              no-throw           IllegalState…      no-throw
shadowedCauseCtorCauseWins              …IllegalArg:ctor   …IllegalState:own  …IllegalArg:ctor
shadowedDetailMessageGetMessage         real               own                real
shadowedCauseOwnFieldUntouched          …IllegalArg:own    …IllegalArg:own    …IllegalArg:own
shadowedCauseSecondInitCauseThrows      IllegalState…      IllegalState…      IllegalState…
shadowedDetailMessageOwnFieldUntouched  own                own                own
```

Note `shadowedDetailMessageGetMessage`: `getMessage()` was returning the
subclass's field, not the message the throwable was constructed with. That is
the same defect on a second field, and it was reachable by any subclass with a
field of that name — it just had no failing test until now.

The **whole** probe — all 872 observables — is byte-for-byte identical to real
HotSpot JDK 25 on the fixed binary. Before the fix it diverged on 5 of them
(10 diff lines); the other 867 were already clean and stayed clean.

**Gates.** Run on the branch and then re-run on **pristine `origin/dev` in the
same worktree and target directory**, because four gates were already red:

| gate | pristine `origin/dev` | branch |
|---|---|---|
| `clippy --workspace --all-targets` | FAIL — `redundant guard` in `cratonvm-jfr` | FAIL, same |
| `test -p cratonvm-vm --lib` | — | PASS |
| `test --workspace` | FAIL — `every_backend_door_goes_through_the_admission_gate` | FAIL, same |
| `check --workspace --features synthetic-jdk` | — | PASS |
| `test -p cratonvm-vm --lib --features synthetic-jdk` | FAIL — 5 tests (4x `cyclic_barrier_*`, `object_output_stream_p70`) | FAIL, **same 5** |
| `test -p cratonvm-native-builtins --lib --features synthetic-jdk` | — | PASS |
| `test -p cratonvm-jit --lib` | PASS | **flaky** — see below |

`tests::recursive_compile_cycle_routes_parent_direct_call_through_dispatch` is
flaky and unrelated: on the branch it is 6/6 PASS run in isolation and
PASS/FAIL/PASS across three full-gate runs. Nothing in this change reaches JIT
compilation.

## What the original report asked, answered

* **Which of the five nested wraps is the first spurious `initCause`?** None of
  them is spurious. H2 calls `initCause` exactly once per `JdbcSQLException`,
  exactly as it does under HotSpot; CratonVM refused every one of those calls.
  The call *count* never diverged — the inference in the original report that
  it must have was wrong, and the per-call trace above is what settled it.
* **Only `TestUpgrade`, or any multi-layer JDBC rewrap?** Neither, and the
  real scope is wider than the question: every H2 `JdbcSQLException` on every
  H2 1.2/1.3/1.4 error path, plus any throwable in any library that declares a
  field named `cause` or `detailMessage`. The bound is "declares a shadowing
  field", not "H2" and not "JDBC".
* **Interpreter dispatch or a native?** A native — field addressing in the
  `Throwable` bridge. Dispatch was never involved.

## Related

* `native-builtins/src/lang_misc.rs` — `shadowed_throwable_slot` and the
  `throwable_field_get`/`throwable_field_set` pair.
* `probes/ShadowDifferentialProbe.java` — the eight regression observables.
