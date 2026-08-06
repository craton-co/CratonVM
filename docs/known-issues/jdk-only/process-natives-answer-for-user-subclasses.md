# `java.lang.Process`'s CONCRETE natives answer for a user subclass, from the wrong object's fields

**Status:** OPEN — measured, with a committed repro. Found 2026-08-06 while
closing the L5 residuals; it is a **behaviour** defect, not a classification
question, and it is the one the L5 record filed in its harmless column.

`register_process_natives` registers all of `java.lang.Process`'s methods —
abstract and concrete alike — under both `java/lang/Process` and
`cratonvm/synthetic/Process`. For a `Process` the **application** subclasses,
four of the concrete ones answer from the VM's synthetic field layout instead of
running `java.lang.Process`'s own bytecode:

| call on a user subclass | HotSpot 25 | CratonVM |
|---|---|---|
| `isAlive()` on a process that has not exited | `true` | **`false`** |
| `pid()` | `UnsupportedOperationException` | **`0`** |
| `toHandle()` | `UnsupportedOperationException` | **returns a handle** |
| `waitFor(0, NANOSECONDS)` on a process that has not exited | `false` | **`true`** |
| `exitValue()` calls those four should have made | 1 | **0** |
| `destroy()` calls `destroyForcibly()` should have made | 1 | **0** |

`isAlive()` answering `false` for a live process is the dangerous one: it is the
standard liveness test, and the wrong answer is the *safe-looking* one. `pid()`
returning `0` breaks the documented contract that an implementation without pid
support must throw, so a caller cannot tell "no pid" from "pid 0".

Contract §1.4 says a `Bridge` loses to real bytecode. These four win against it.

## Why the abstract registrations are inert and the concrete ones are not

The L5 record has this exactly backwards, and it is worth stating because the
inverted model is the reason nobody looked here:

> The abstract ones intercept **every** implementor, including a user subclass
> of `Process` — the same hazard `register_interface_natives` carries.

The **abstract** registrations (`waitFor()I`, `exitValue`, `destroy`,
`getInputStream`, `getErrorStream`, `getOutputStream`) are inert *because* the
method is abstract: a concrete subclass **must** override it, so receiver-driven
dispatch finds the override and never walks up to `java/lang/Process`. Measured:
all six reach the application's bytecode.

The **concrete** registrations are the hazard, for the mirror-image reason: a
subclass normally does *not* override `isAlive`/`pid`/`toHandle`/`waitFor(J,TU)`,
so dispatch walks its class chain, reaches `java/lang/Process`, and finds the
native — which is registered there and wins.

So "abstract ⇒ intercepts everything" is the wrong test for this hazard. The
right one is **"is there an inherited registration for a method this class does
not override"**, and it selects the opposite set.

## The mechanism

`native_process_wait_for_timeout` (`native-io/src/process.rs`) is the clearest
case:

```rust
let handle = handle_of(ctx, this);          // get_field(this, PROC_FIELD_HANDLE)
if handle == 0 {
    let exited = !matches!(
        ctx.get_field(this, PROC_FIELD_EXIT), Value::Int(EXIT_NOT_YET));
    return Ok(Some(Value::Int(if exited { 1 } else { 0 })));
}
```

`PROC_FIELD_HANDLE` and `PROC_FIELD_EXIT` are **fixed slot indices** into the
layout `spawn_and_wrap` allocates for `cratonvm/synthetic/Process`
(`PROC_FIELD_COUNT` = `JAVA_PROCESS_FIELD_COUNT + 6`). A user subclass has an
unrelated layout, so both reads land on someone else's fields — or off the end.
`handle_of` answers `0`, the `EXIT_NOT_YET` sentinel does not match whatever was
read, and the native concludes the process exited.

This is the `fabricated-object-layouts-leak-into-native-code.md` family: an
indexed field read is only meaningful against the layout it was written for, and
nothing at the registration site says which receivers that is.

## Reproduction

`probes/UserProcessInterceptProbe.java`, committed with this record — the shape
`UserImplementorInterceptProbe` established for `Map`/`Collection`, applied to a
`Process`. It prints every observable as `key=value` so a run diffs byte-for-byte
against HotSpot, and it counts calls into the subclass, because **every return
value in the first section matches while the counters do not**: that is how the
defect was found at all.

```sh
javac -d out probes/UserProcessInterceptProbe.java
java  -cp out UserProcessInterceptProbe                       # control
cratonvm --real-jdk --java-home "$JDK" -cp out UserProcessInterceptProbe
```

Two `VERDICT=` lines and the `concrete.*` block are the assertion.
`stillRunning.VERDICT` reads
`A-NATIVE-ANSWERED-WITHOUT-ASKING-THE-SUBCLASS` today.

## What would close this

The registration on `java/lang/Process` cannot simply be dropped: it is what
makes the VM's own synthetic `Process` reachable through a
`java.lang.Process`-typed reference, which is how every caller holds one
(`Process p = pb.start()`), and the record above it says removing it once raised
`NoSuchMethodError` on exactly that path.

So each of the four needs a **receiver check** — serve the synthetic layout only
when `class_id_of_object(this)` is `cratonvm/synthetic/Process`, and otherwise do
what `java.lang.Process`'s bytecode does, which for three of them is one line:

* `toHandle()` → throw `UnsupportedOperationException`
* `pid()` → `toHandle().pid()`, i.e. also throw for a subclass that has no handle
* `destroyForcibly()` → `destroy(); return this;`
* `isAlive()` / `waitFor(long, TimeUnit)` → poll `exitValue()` through
  `invoke_virtual`, treating `IllegalThreadStateException` as "not exited"

Deliberately **not** done in the change that filed this: it is five natives on
the subprocess path WildFly depends on, the guard wants applying to the whole
registrar rather than to the methods this probe happened to cover, and a fix
belongs with its own verification of the real-subprocess arm (which this probe
does not exercise — it never spawns anything).

**The other receiver of these same registrations** is
`cratonvm/synthetic/Process`, for which reading `PROC_FIELD_*` by fixed index is
correct and necessary — that object does not extend `java.lang.Process` and
inherits nothing from it, so the natives are the only reason it answers at all.
The two records are one registration seen from its two receivers; see
[`synthetic-process-cluster-and-the-supertype-lie.md`](synthetic-process-cluster-and-the-supertype-lie.md),
which is why the guard has to be a receiver check and not a deletion.

**The generalisable test**, from the contrast with `native-collections`' abstract
family (which is measured inert for arbitrary layouts): does the native **ask the
receiver**, or does it **index into a layout it assumes**? The collections
natives call `size()`/`toArray()`/`iterator()` and survive any subclass; these
read `PROC_FIELD_HANDLE` and believe what they find.

**The general question underneath it** is the one
`native-kind-is-ambient-and-defaults-to-syntheticstub.md` and
`ensure-synthetic-class-cannot-enforce-only-record.md` both circle: a native
registered on a JDK class name, implemented against a VM-minted layout, has no
way to say "only for my own objects". Until it does, every such registration is
one inherited dispatch away from reading a stranger's fields.
