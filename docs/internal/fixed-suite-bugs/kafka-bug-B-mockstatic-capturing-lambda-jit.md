# Bug B residual — `mockStatic` capturing-lambda stub bypassed by JIT (fixed)

| | |
|---|---|
| **Severity** | Medium (blocks the last Kafka `ClientUtilsTest` reverse-lookup case; narrow trigger) |
| **Kind** | JIT correctness — stale call target after JVMTI class redefinition |
| **Surfaced by** | `org.apache.kafka.clients.ClientUtilsTest.testParseAndValidateAddressesWithReverseLookup` (`ksuite/repro/MockClientUtils.java`, line 17) |
| **CratonVM** | Fixed on `dev` · previously failed with JIT on · **HotSpot** OK |
| **Status** | FIXED 2026-07-01 — redefine now quiesces compiled dispatch paths after class redefinition |
| **Predecessor** | The dispatch/shadowing half of Bug B is **FIXED on `dev`** — see [`kafka-bug-B-mockito-mockstatic-mock-dispatch.md`](kafka-bug-B-mockito-mockstatic-mock-dispatch.md) |

## Resolution (2026-07-01)

Fixed on `dev` by `353e93c0`, merged via `25fd5d11`: class redefinition now clears compiled
methods and JIT dispatch helpers/interpreter paths stop using cached or newly-published compiled
targets once any class has been redefined. This implements the conservative blanket quiesce option
recommended below.

## Summary

After the Bug B dispatch + native-shadow fixes landed on `dev` (set-match redefine,
shadow-suppression for redefined classes, retransform-from-original, and the cache-hit
shadow eviction for the `mockStatic`+`mock` combination), `MockClientUtils` advances from
its first failure (line 14, the instance mock) to **line 17**, the `mockStatic` stub of the
**static** `InetAddress.getAllByName`:

```java
inet.when(() -> InetAddress.getAllByName(hostname)).thenReturn(new InetAddress[]{a1, a2});
//                                       ^^^^^^^^ captured local variable
```

This throws `MissingMethodInvocationException` — the lambda's `getAllByName` call is not
registered as a `MockedStatic` invocation, so `when()` has nothing to stub.

## The trigger is a **capturing lambda**, and the cause is the **JIT**

Two independent, narrow conditions are both required:

1. **The mockStatic expression is a *capturing* lambda.** A literal argument works; a
   captured local does not.
2. **The JIT is on.** `--nojit` makes the captured case pass.

Bisection (`ksuite/repro/BisectProbe.java`, run on the post-fix `dev` binary):

| Variant | Lambda | Result (JIT on) |
|---|---|---|
| V1 | `() -> getAllByName(capturedVar)`, empty return | **FAIL** `MissingMethodInvocationException` |
| V2 | `() -> getAllByName("literal")`, mock return | OK |
| V3 | `() -> getAllByName(capturedVar)`, mock-array return | **FAIL** `NullPointerException` |

`ksuite/repro/LamProbe.java` isolates the JIT axis:

```
captured, JIT on   -> FAIL (MissingMethodInvocationException)
captured, --nojit  -> OK   (stub registered)
literal,  --nojit  -> OK
```

`InetAddress.getAllByName` is **plain JDK bytecode** (no registered native, not in the
force-native set), so this is **not** the native/intrinsic shadowing addressed by the
landed fixes. The woven static-mock interception is being bypassed by JIT-compiled code.

## Root cause

When Mockito redefines `InetAddress` (the `mockStatic` retransform weaves its static
methods), CratonVM's redefine path calls `fire_jit_invalidate_hook(class_id)`, which evicts
JIT compilations **of the redefined class's own methods**. It does **not** invalidate the
JIT **dispatch caches of *caller* methods** that already baked a direct/cached call to
`InetAddress.getAllByName`. A JIT-compiled caller (the capturing lambda body and/or the
Mockito `when()` machinery exercised on this path) therefore keeps dispatching the
**pre-redefine** (un-woven) `getAllByName`, so the static-mock advice never runs and no
invocation is recorded.

The literal lambda differs only in *how* CratonVM realizes it (a non-capturing lambda is
served from a cached singleton whose body stays interpreted on this path), which is why the
literal case happens to dodge the stale JIT'd call site.

### Attempted fix that did **not** work (and what it rules out)

A guard was added at the top of `jit_invoke_dispatch` (`vm/src/jit/helpers.rs`): when
`any_class_redefined()` and `info.class_name`'s `class_redefine_generation > 0`, bail to the
interpreter (whose dispatch gates re-resolve to the woven body). This is the JIT analog of
the interpreter-side cache-hit shadow eviction that fixed the `mockStatic`+`mock`
combination — but it **did not** fix this case and was reverted.

That it has no effect means the offending `getAllByName` call is **not** routed through
`jit_invoke_dispatch` at runtime: it is compiled as an **inlined or directly-bound call** in
the JIT'd caller, so there is no per-call dispatch helper to gate. The fix must therefore
act at **redefine time**, not dispatch time.

## Recommended fix direction

On JVMTI redefine of a class `C`, in addition to evicting `C`'s own JIT compilations:

- **Flush the JIT dispatch / inline caches and direct-bound call sites that target `C`'s
  methods** in *other* compiled methods (caller-side invalidation), **or**
- **De-optimize** (discard compiled code for) any method whose compiled body contains a
  bound call into `C`, **or**
- The conservative blanket option: while **any** class is redefined (`any_class_redefined()`
  is set — i.e. a mock/agent session is live), **stop binding direct/inlined calls in newly
  compiled code** and route through the gated dispatch helper so the existing
  `jit_invoke_dispatch` redefine-bail can take effect. Mock-heavy test runs are not
  perf-critical, so the cost is acceptable and scoped to redefine sessions.

A precise caller-side invalidation is the correct long-term fix; the blanket option is the
cheapest path to making the Kafka case pass.

## Repro

All under `ksuite/repro/` (run with a JDK ≥ 19 boot, `--java-home` pointing at it):

- `MockClientUtils.java` — the real `ClientUtils.parseAndValidateAddresses` case (fails at line 17 on `dev`, JIT on).
- `LamProbe.java <literal|captured>` — minimal `mockStatic` static stub; toggle `--nojit` to see the JIT axis.
- `BisectProbe.java` — V1/V2/V3 isolating captured-var vs literal and the return-value shape.
- `SeqProbe.java` / `ColdProbe.java` — confirm the instance-mock count and ordering are **not** the trigger (literal works at all counts).

Workaround for affected suites today: run with `--nojit`.

## Related

- [`kafka-bug-B-mockito-mockstatic-mock-dispatch.md`](kafka-bug-B-mockito-mockstatic-mock-dispatch.md) — the (now fixed) dispatch/shadowing half of Bug B.
- The JIT-cache-vs-redefinition theme overlaps the JIT MIC/PIC families in
  [`jit-regalloc-callee-saved-clobber-family.md`](../jit-regalloc-callee-saved-clobber-family.md)
  and the reflection-corruption GC-root family only insofar as both are "JIT caches a stale
  fact across a runtime structural change"; the fix here is specifically redefine-time cache
  invalidation.
