# RBC.6's `getfield`/`putfield` admission lets an NPE escape a handler that catches it — and its own acceptance probe says so in one run

**Status: OPEN, reproduced 2026-08-18, not fixed.** Found while validating the
`new`/`athrow` admission (`performance/netty-adaptive-allocator-throughput-FIXED-20260817`),
by running the sibling probe as a regression control. It is **not** a regression
from that work: the same binary built from the branch point fails identically,
and so does every intermediate build.

## The bug

`probes/Rbc6FieldProbe.java` is the acceptance test that
`precise_field_ops_enabled` names in its own doc comment — "five methods whose
handler reads a non-parameter local written inside the `try`, differentially
checked against HotSpot and `--nojit`". Run it today and the JIT arm does not
produce a wrong answer; it produces **no answer at all**:

```
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/NullPointerException
        at Rbc6FieldProbe.main(Rbc6FieldProbe.java:88)
```

Line 88 is `acc += getfieldIntHandlerLocal(h, i);` — a call to a method whose
entire body is a `try`/`catch (NullPointerException)`. The NPE it is supposed to
catch propagates out of it instead.

## It is exactly the admission, named by its own kill switch

One binary, one variable — the opt-out that
`precise_field_ops_enabled` documents for precisely this purpose:

| arm | result |
|---|---|
| HotSpot 25 | `acc=3505302427599075008` + all five spot values correct |
| CratonVM `--nojit` | `acc=3505302427599075008` — identical |
| CratonVM, 10 iterations (never compiles) | correct |
| CratonVM JIT, default | **uncaught NullPointerException** |
| CratonVM JIT, `CRATONVM_JIT_NO_PRECISE_FIELD_OPS=1` | `acc=3505302427599075008` — identical |

The last two rows differ by nothing but whether `getfield`/`putfield` (0xb4/0xb5)
are admitted to RBC.6. **Withdrawing the admission makes it correct.**

The iteration count matters and is the reason this is not caught by a short
run: 10 iterations pass, 200 000 fail. The methods have to cross the C1
threshold and be entered as compiled code with the handler live before the
defect appears, which is the population the admission exists to serve.

## Why this matters more than one probe

`precise_field_ops_enabled` was landed 2026-08-02 on the argument that both
opcodes ALREADY published a precise exceptional frame on every throwing path,
so the admission was bookkeeping rather than new codegen —
`fixed-suite-bugs/rbc6-protected-field-ops-FIXED-20260802.md` carries that
argument in full, path by path. One of those paths does not hold, or does not
hold in the state the compiled entry reaches it in.

The population is every `try`-wrapped field access in the tree. A method that
compiles and then fails to route an NPE to its own handler is a **silent wrong
answer** wherever the handler would have returned a value, and an escaping
exception only where it would not — which is why this reads as a crash here and
would read as a wrong number elsewhere.

## What the next attempt should NOT assume

The sibling admission for `new` (0xbb) and `athrow` (0xbf), landed 2026-08-17,
is separately validated and is **not** implicated: `probes/Rbc6AllocThrowProbe.java`
matches HotSpot across 200 000 iterations in all four arms while
`hot_but_stuck_in_interpreter` proves its five methods actually compile (5
refused with the admission withdrawn, 0 with it on). The two admissions publish
their frames through different emitters — `emit_post_alloc_oom_check` and the
reason-9 `athrow` stub, versus `getfield`'s four sub-path receiver checks and
`emit_precise_null_check_field_store`. Do not fix one by reasoning from the
other.

Start instead from the four `getfield` sub-paths that the 2026-08-02 argument
enumerates (compact-inline, uniform-inline, resolved-helper, unresolved-helper)
and ask which one a COMPILED entry actually selects for this shape, then whether
`emit_post_invoke_exception_check` really follows it there.

## Repro

```bash
javac -d /tmp/p probes/Rbc6FieldProbe.java
JDK=<jdk25>
<cv-bin> --java-home "$JDK" --Xmx 1500m -cp /tmp/p Rbc6FieldProbe 200000   # NPE escapes
CRATONVM_JIT_NO_PRECISE_FIELD_OPS=1 \
<cv-bin> --java-home "$JDK" --Xmx 1500m -cp /tmp/p Rbc6FieldProbe 200000   # correct
<cv-bin> --nojit --java-home "$JDK" --Xmx 1500m -cp /tmp/p Rbc6FieldProbe 200000  # correct
"$JDK/bin/java" -cp /tmp/p Rbc6FieldProbe 200000                            # the control
```

`acc=3505302427599075008` is the value all three correct arms agree on.
