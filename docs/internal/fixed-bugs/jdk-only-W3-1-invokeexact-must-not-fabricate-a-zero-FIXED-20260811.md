> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkHandles` passes in the 53/1 run. The out-of-file patch this record said the check depends on IS applied — `vm/src/runtime/interpreter/invoke.rs:3541` routes through `unbox_poly_return_checked`, which is the funnel the failing call site uses. The record's status line ("behind an opt-in flag that defaults to today's behaviour") is **stale**: `CRATONVM_MH_STRICT_INVOKEEXACT` was flipped on 2026-08-07 and `vm/src/vm/vm_exec.rs:1588` now reads it as an opt-OUT (`=0` restores the old behaviour). The decision record for that flip, W5-4, is retired alongside this one.
>
> Previous location: `docs/known-issues/jdk-only/W3-1-invokeexact-must-not-fabricate-a-zero.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `MethodHandle.invokeExact` fabricated a zero instead of `WrongMethodTypeException`

**Status:** FIXED in source 2026-08-07 (lane W3-1, JDK-only wave 3), **behind an
opt-in flag that defaults to today's behaviour**. Not verified against a binary
— see *How to verify*. Requires **out-of-file patches** (listed below); without
the `vm/src/runtime/interpreter/invoke.rs` one the check never fires, because
that is the funnel the failing call site actually uses.

This is the second-order failure lane W2-5 diagnosed and deliberately did not
fix; see `W2-5-methodhandles-arrayelement-combinators.md` §"The next failure".

## The failure

`regression-suite/src/RJdkHandles.java` fails in **both** `--real-jdk` and
`--jdk-only` with a byte-identical trace; HotSpot 25 runs it to exit 0.

```
CK RJdkHandles invoke ok type=(int,int)int
Exception in thread "main" java/lang/AssertionError: unreachable
    at RJdkHandles.main(RJdkHandles.java:293)
    at RJdkHandles.adaptation(RJdkHandles.java:153)
    at RJdkHandles.check(RJdkHandles.java:26)
```

`adaptation()` lines 149-157:

```java
// A wrong invokeExact descriptor is a linkage-time error, not a silent coercion.
boolean threw = false;
try {
    long bogus = (long) add.invokeExact(1, 2);   // line 152
    check(bogus == 3, "unreachable");            // line 153
} catch (WrongMethodTypeException expected) {
    threw = true;
}
check(threw, "invokeExact with the wrong descriptor must throw …"); // line 157
```

## The rule, established from `javap` and the spec

`invokeExact` requires the call site's **symbolic descriptor** to be identical
to the handle's `type()` — no widening, no boxing, no `asType`. Any difference
is a `WrongMethodTypeException`. `invoke` is the opposite: it is *specified* to
apply `asType` conversions. `invokeBasic` and the `VarHandle` accessors are
likewise permissive.

`javap -c -p RJdkHandles` on a JDK 25 build of the file confirms which is which.
`adaptation()` alone contains both, and they must keep behaving differently:

| bytecode offset | call site | required behaviour |
|---|---|---|
| 570 | `MethodHandle.invokeExact:(II)J` on a `(II)I` handle | **throw** `WrongMethodTypeException` |
| 440 | `MethodHandle.invoke:(IIII)I` on a `(int[])int` varargs collector | convert (`asVarargsCollector`) |
| 494 | `MethodHandle.invoke:([I)I` | convert (`asFixedArity`) |
| 545 | `MethodHandle.invokeExact:([II)I` | match — no throw |
| 196 | `MethodHandle.invokeExact:(Ljava/lang/Integer;Ljava/lang/Integer;)Ljava/lang/Integer;` | match — no throw |

So a check that made `invoke` strict would break offsets 440 and 494 in the very
same method. It is gated on the method name for exactly that reason.

## What we did instead: a silent wrong answer

`add` is `lk.findStatic(…, methodType(int, int, int))`, so its `MH_DESC` is
`(II)I`. `register_t4_method_handle_invoke`'s `invokeExact` native dispatches it
and then calls `auto_box_return(ctx, result, &desc)` with the **handle's** desc
— producing a boxed `java/lang/Integer(3)`.

The interpreter then unboxes against the **call site's** descriptor, `(II)J`.
`coerce_value_against_ret_char` (`vm/src/vm/vm_exec.rs`) reaches:

```rust
if cls_name != expected_wrapper {
    return match ret_char {
        b'J' => Value::Long(0),
        …
    };
}
```

`java/lang/Integer` is not `java/lang/Long`, so the call yields `0L`. No
exception anywhere: `bogus == 0`, the `check` fires, and the JDK's specified
`WrongMethodTypeException` never happens.

**Note the shape of that branch:** every value that reaches it is *already* a
wrong answer. The wrapper mismatch means the coercion has given up and is
inventing a zero. Nothing correct is being produced there today.

## The fix

`vm/src/vm/vm_exec.rs` gains `unbox_poly_return_checked(shared, thread, value,
descriptor, method_name)`, a wrapper around `unbox_poly_return` that throws
`java/lang/invoke/WrongMethodTypeException` on **one** shape:

* `method_name == "invokeExact"` (never `invoke` / `invokeBasic` / VarHandle), and
* `CRATONVM_MH_STRICT_INVOKEEXACT` is set, and
* the call-site return char is a **primitive**, and
* the native produced a **boxed primitive** whose wrapper class is one of the
  eight in `PRIMITIVE_WRAPPER_CLASSES` and is **not** the one the return char
  names.

That is the exact predicate of the "fabricate a zero" branch above, and nothing
wider. Everything else — a `null` result, a non-wrapper object, an un-nameable
fabricated class, a reference/array/void return, any argument-side mismatch —
takes today's path unchanged.

Two deliberate softenings:

* If `java/lang/invoke/WrongMethodTypeException` will not load (a
  `synthetic-jdk` build need not model it), the function degrades to today's
  fabricated zero rather than replacing a wrong answer with an uncatchable
  internal abort.
* The check is **return-type only**. `register_t4_method_handle_invoke` carries
  an in-tree warning that a strict *arity* check there once aborted the VM and
  killed every Groovy `IndyInterface` call site
  (`WrongMethodTypeException: expected 2 args, got 1`), because CratonVM's
  `MH_KIND_*` adapters keep their inner target's descriptor and their apparent
  arity routinely disagrees with the real call site. That objection does not
  transfer to the *return* type, which those adapters preserve — but it is the
  reason no argument check was added.

So this is strictly **less** strict than HotSpot. The defect being fixed is the
silent wrong answer, not full spec conformance.

## The flag

`CRATONVM_MH_STRICT_INVOKEEXACT`, declared as
`CRATONVM_COMPAT=mh-strict-invokeexact`. **Default off — unset, every path is
byte-identical to before.**

It is read through `cratonvm_types::flags::runtime_var_os`, which serves
declared `CRATONVM_*` names from the one immutable `VmFlags` snapshot, and then
memoised in a `OnceLock`. It therefore **latches twice**: once when the snapshot
is built at VM start, and again on first read here. Exporting it before
launching the process is the only way to set it; an in-process `set_var` after
any flag has been read is invisible.

## Blast radius

`unbox_poly_return` is the funnel for **every** signature-polymorphic call site
in the VM. Four call sites now route through the checked wrapper:

| site | reached by |
|---|---|
| `vm/src/runtime/interpreter/invoke.rs` (`try_stackless_invoke`) | **the interpreter's `invokevirtual` on `MethodHandle` — the production path** |
| `vm/src/vm/vm_exec.rs` ×3 (`invoke_on_class_shared_inner`) | `DowncallHandle`-preferred, base-class and exact-class native lookups (JNI / reflective / lambda dispatch) |

`vm/src/runtime/invokedynamic.rs:1053` calls `coerce_value_against_ret_char`
directly and is **not** touched — indy call sites are not `invokeExact`.

At risk, and worth re-running in the A/B with the flag **on**: Groovy
(`IndyInterface`, which invokes its dispatch chains through `invokeExact`),
JRuby, Panama (`DowncallHandle`, the float/`MemorySegment` paths), Jackson 3
(`DirectMethodHandle$Constructor`), Netty (`invokeExact(Thread)Z`), and
Spring/Hibernate proxying. The shape to watch for is a **new**
`WrongMethodTypeException` where the run previously produced a zero/`false`.
Under `Z`, in particular, today's fabricated `Int(0)` reads as `false`; with the
flag on, a mismatched wrapper there throws instead.

## How to verify

```
cargo build --release -p cratonvm-cli
# A — default, must be unchanged
target/release/cratonvm --real-jdk -cp regression-suite/classes RJdkHandles
# B — strict
CRATONVM_MH_STRICT_INVOKEEXACT=1 target/release/cratonvm --real-jdk -cp regression-suite/classes RJdkHandles
CRATONVM_MH_STRICT_INVOKEEXACT=1 target/release/cratonvm --jdk-only  -cp regression-suite/classes RJdkHandles
```

Arm A must still fail at `RJdkHandles.java:153`. Arm B must reach
`CK RJdkHandles adapt=14` and `PASS RJdkHandles (51 checks)`.

## The falsifying observation

If arm B fails with `AssertionError: invokeExact with the wrong descriptor must
throw WrongMethodTypeException` at **line 157** instead of line 153, this design
is wrong: it would mean the `invokeExact` native returned a raw `Value::Int`
rather than a boxed `Integer`, so `coerce_value_against_ret_char` widened it to
`Long(3)`, `bogus == 3` held, and the value-shape check had nothing to fire on.
Fixing it would then require the handle-descriptor check W2-5 originally
suggested (compare `MH_DESC`'s return token against the call-site return char),
which is a materially broader gate — it fires even where dispatch produced the
right value — and would need its own lane.
