# A `VarHandle` access with a null coordinate answers instead of throwing

## Status
**OPEN, opened 2026-09-01.** Found while writing the hot vector for the
`VarHandle` reference-read bind
(`performance/juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`);
it is not caused by that bind and reproduces with every `VarHandle` access on
the generic funnel.

## Symptom

HotSpot raises `NullPointerException` when an instance-field `VarHandle` is
accessed with a null coordinate. CratonVM answers a value, and for a write it
answers by doing nothing.

```java
static class H { Object any = "x"; int i = 5; }
VarHandle ANY = lookup().findVarHandle(H.class, "any", Object.class);
VarHandle INT = lookup().findVarHandle(H.class, "i", int.class);

ANY.get((H) null);        // HotSpot: NullPointerException   CratonVM: null
(int) INT.get((H) null);  // HotSpot: NullPointerException   CratonVM: 0
ANY.set((H) null, "y");   // HotSpot: NullPointerException   CratonVM: returns normally
```

Measured on JDK 25 (`/data/toolchain/jdk-25`) against CratonVM at `267549501`,
three iterations each, identical every time on both VMs.

**The `set` is the worst of the three.** A read that answers `null` at least
hands the caller something it may check; a write that silently does nothing
loses the store with no signal anywhere, and the next read of that field
returns a stale value that looks legitimate. It is the same species as
`a swallowed introspection error deletes every annotation` — a loud failure
converted into a silent wrong answer.

## Severity
**MEDIUM.** No workload in the corpus is known to hit it, because well-typed
code does not read through a null coordinate on purpose. The exposure is
defensive code that relies on the NPE — `Objects.requireNonNull`-by-side-effect
patterns, and any library that catches NPE to detect a torn or uninitialised
receiver. It is a silent-wrong-answer species, which is why it is filed rather
than left in a commit message.

## Where it is

`native-builtins/src/lang_invoke.rs`. Each access mode decodes the coordinate
out of `args[1]` and falls out of the match on anything that is not a live
object. `varhandle_get`'s instance arm is the shape:

```rust
let receiver = match args.get(1) {
    Some(Value::Object(Some(r))) => *r,
    _ => return Ok(Some(Value::Object(None))),
};
```

The same decode, with the same "answer instead of raise" fallthrough, appears
at least at the array-element and `ByteBuffer`-view arms of `varhandle_get`,
at `varhandle_set`, and at the `getAndSet` / `getAndAdd` / `getAndBitwise` /
`compareAndExchange` arms beside them. **Fixing one is worse than fixing none**
— an NPE from `get` and a silent no-op from `set` on the same null receiver is
harder to diagnose than today's uniform silence.

The JIT's thin direct binds are NOT the cause and do not need changing: all
three (`VARHANDLE_READ_DIRECT_FNS`, `..._WRITE_...`, `..._CAS_...`) test
`receiver != 0` and decline to the funnel, so the funnel's answer is the only
answer. Verified by running the repro with
`CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT=0`: byte-identical output.

## Why it was not fixed with the bind that found it

Three reasons, recorded so the next person does not re-litigate them:

1. it is **pre-existing and orthogonal** — it reproduces with every bind off;
2. the fix is **not local**. Doing it once means every access mode and every
   handle kind, or the result is worse than the current uniform wrongness;
3. it **changes behaviour for code that currently passes**. Anything in the
   Spring Boot, netty, hibernate or H2 suites that today reaches a null
   coordinate and quietly gets `null` would start throwing. That needs a suite
   pass on both platforms, which is a piece of work with its own gates.

## Next step

1. Enumerate the decode sites — `grep -n "args.get(1)" native-builtins/src/lang_invoke.rs`
   and read each match's fallthrough arm. Enumerate and DIFF against HotSpot
   rather than fixing the ones that look wrong: the lesson from
   `a design note saying every method must be overridden is not a list` is that
   the set is what matters, not the instances anyone happened to notice.
2. Decide the arity question at the same time: HotSpot also raises
   `WrongMethodTypeException` for a coordinate COUNT mismatch, which this tree
   has never checked. That is a different rule and should not be smuggled in.
3. Land behind a kill switch, then run the four suites on both platforms.

## Repro

```bash
javac -d <out> NullCoord.java     # the three lines under "Symptom"
java     -cp <out> NullCoord      # three NullPointerExceptions per round
cratonvm --java-home <jdk> -cp <out> NullCoord   # null, 0, and a silent no-op
```

## Related

- `performance/juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md` (internal)
  — the work that found it, and the reference-read bind's own gates.
- `regression-suite/src/RJitVarHandleRefRead.java` carries a comment at the
  point where this assertion WOULD go, naming this page. Add the assertion when
  this closes; the vector already runs hot enough to cover both routes.
