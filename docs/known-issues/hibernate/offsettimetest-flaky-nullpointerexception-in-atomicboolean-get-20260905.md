# `OffsetTimeTest` — a flaky, parameter-independent `NullPointerException` inside `AtomicBoolean.get()`

## Status

**OPEN, narrow, and confirmed flaky rather than timezone-specific.** Not
pinned to a CratonVM source line — the exception's own stack trace has only
one frame and no caller — but re-running the exact same class three times
produced the failure at two different parameter indices and, once, not at
all, which rules out the specific timezone/offset parameter the harness's
first report implicated and points instead at a narrow, low-probability
concurrency-shaped defect.

## The symptom, as first reported

2026-09-05 Generational-GC hib-orm rerun
(`nonpassed-rerun-gen-20260905/run-20260905-182403-passed/on-real/shard-0/`):

```
JUnit Jupiter:OffsetTimeTest:[73] Parameter[env=[JVM TZ: Europe/Paris, JDBC TZ: null, remapping dialect: null], data=DataImpl[hour=2, minute=0, second=0, nanosecond=0, offset=-01:00, yearWhenPersistedWithoutHibernate=1970, monthWhenPersistedWithoutHibernate=1, dayWhenPersistedWithoutHibernate=1]]:nativeWriteThenRead(SessionFactoryScope)
=> java.lang.NullPointerException
```

No message, no stack trace in that run's `raw.log` — `CratonRunner`
(`apps/hib-suite-runner/CratonRunner.java`) does call
`summary.printFailuresTo(new PrintWriter(System.err), 50)` when `failed > 0`,
so a stack trace should be there in principle; this class's 396 sub-invocations
apparently made it easy to lose in a `grep` without enough context. Re-running
in isolation gets it directly.

Index `[73]`, `Europe/Paris`/`offset=-01:00` is the
`.add( 2, 0, 0, 0, "-01:00", ZONE_PARIS )` row in `OffsetTimeTest.testData()`
— one of the `HHH-13379` DST-boundary-edge-case rows this class carries. That
proximity to `hib-temporal-localdatetime-offsettime-one-hour-skew-FIXED.md`
(a real, previously-fixed CratonVM timezone-table bug affecting exactly this
class) made "another timezone-table gap" the obvious first hypothesis. It is
not what this is — see below.

## Getting more evidence: three isolated reruns of the whole class, same binary, same host

```bash
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 300 <gen-wrapper> --java-home <jdk25> \
  @common.args -Dcraton.batch=1 CratonRunner org.hibernate.orm.test.type.temporal.OffsetTimeTest
```

| run | result | failing case |
|---|---|---|
| original harness rerun | `found=396 ... failed=1` | `[73]` `nativeWriteThenRead`, `Europe/Paris`, offset `-01:00` |
| isolated rerun 1 | `found=396 started=264 ok=175 failed=1 aborted=88 skipped=132` | `[107]` `writeThenRead`, `Pacific/Auckland`, offset `+02:00` — **different method, different parameter, different timezone family entirely** |
| isolated rerun 2 | `found=396 started=264 ok=176 failed=0 aborted=88 skipped=132` | **no failure at all** |

`testData()` builds its parameter list from a fixed, non-randomized sequence
of `.add(...)` calls, so parameter *enumeration* order is stable — the
differing failure index across runs is the class genuinely failing at a
different point each time, not an artifact of unstable indexing. This
**rules out** the timezone-table-gap hypothesis (which would predict the
*same* parameter failing every time) and rules out any hypothesis tied to
`Europe/Paris`, DST boundaries, or the `nativeWriteThenRead` method
specifically — isolated rerun 1 failed on a completely unrelated parameter
and method (`writeThenRead`, `Pacific/Auckland`, standard offset, no DST
edge case), and isolated rerun 2 didn't fail at all.

## What both captured failures have in common

Both captured `NullPointerException`s (the original and isolated rerun 1)
carry the **same single-frame stack trace**:

```
java.lang.NullPointerException
	java.util.concurrent.atomic.AtomicBoolean.get(AtomicBoolean.java)
```

No caller frame is present in either. That is itself unusual: on a
standards-conforming JVM, calling `.get()` on a null `AtomicBoolean`
reference throws `NullPointerException` at the **call site**, before the
callee's body ever runs — the top frame of a "real" such NPE would normally
be the *caller*, not `AtomicBoolean.get()` itself. Seeing `AtomicBoolean.get()`
as the (only) frame suggests either CratonVM's stack-trace capture is losing
the caller frame (a known general class of issue in this VM — bare NPEs with
truncated traces are a recurring theme), or the actual fault happens inside
some CratonVM-internal path that synthesizes this frame.

CratonVM's own native implementation of `AtomicBoolean.get()`
(`native_ab_get`, `native-builtins/src/util_concurrent_ext.rs:8279`) does
**not** throw `NullPointerException` for a null receiver — it silently
returns `false`:

```rust
pub(crate) fn native_ab_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),   // <- null/non-object "this" -> false, no throw
    };
    ...
}
```

(`native_ab_set`, `native_ab_cas` and siblings follow the same pattern.)
Under correct JVM semantics a null receiver should never reach this native at
all — `invokevirtual`'s null check happens in the interpreter/JIT dispatch
*before* any native method body runs — so this fallback is normally dead
code, and it is *not* the origin of the observed exception (it cannot
produce one). The real NPE is being raised somewhere in CratonVM's
interpreter/JIT invoke-dispatch path itself, on a call whose target
(`AtomicBoolean.get()`) happens not to preserve the caller's frame in the
resulting trace.

## Narrowed to: flaky, not parameter-specific, not timezone-related

Given:

- the failure moves to an unrelated parameter/method/timezone between runs,
- one of three runs did not fail at all,
- the only common signature is the same bare, caller-less NPE inside
  `AtomicBoolean.get()`,

this reads as a narrow, low-probability, timing/ordering-sensitive defect
somewhere in CratonVM's own invoke-dispatch or object-lifecycle handling
(candidates not individually confirmed: a JIT inline-cache/dispatch-site
issue akin to the kind this session's git history shows being fixed
elsewhere — e.g. `1af0b2d7f "a lambda site must refill an inline-cache slot
its caller recompiled"` — or a GC-timing-dependent transient null observed on
some internal `AtomicBoolean` the test framework or connection pool uses
across the parameterized run's ~264 started sub-invocations). None of these
is confirmed; this section names them as the shape of plausible causes, not
as findings.

## What was checked and found NOT to already cover this

`docs/known-issues/hibernate/` and the internal `fixed-suite-bugs/hibernate/`
tree were searched for `OffsetTimeTest` and `AtomicBoolean` before writing
this page. The only prior `OffsetTimeTest` hit
(`hib-temporal-localdatetime-offsettime-one-hour-skew-FIXED.md`) is the
already-fixed, unrelated timezone-table bug named above — same class, a
different bug, already closed. No existing page covers a bare
`AtomicBoolean.get()` NPE.

## Disposition

1 failure in ~264-396 sub-invocations, non-reproducible on demand (2 of 3
isolated reruns failed, 1 did not, each at a different point), with no caller
frame to work from — this is exactly the "bare NPE, no more information
available" case the triage brief said not to force into a page beyond
recording it. Recorded here, left open, not chased further this session.
Anyone picking this up next should start by trying to get a caller frame
(e.g. a JIT `--nojit` A/B to see if it still reproduces interpreted-only,
which would at least rule the JIT dispatch path in or out) before spending
time on Hibernate/temporal-specific theories — nothing here points at
timezone or temporal-type handling at all.
