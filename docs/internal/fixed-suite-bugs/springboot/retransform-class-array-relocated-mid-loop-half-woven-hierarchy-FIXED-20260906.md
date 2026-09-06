# A moving GC relocated the agent's `Class[]` mid-loop, and half the mock's hierarchy was never instrumented

Status: **FIXED (2026-09-06).** `NestedUrlConnectionTests` 11/11, and the
minimal reproducer 0 failures in 140 processes where it was ~12% before.

This retires the page that carried the last open row of the
`loader-jar-nested-url-connection-npe-pair-FIXED-20260905` write-up. That page
recorded the rate honestly and named one hypothesis. **The hypothesis was
wrong and so was the page's own title.**

## What the title got wrong

The page called it a self-call problem: `getContentLength()` is
`willCallRealMethod`, so the theory was that its inner call to the stubbed
`getContentLengthLong()` was not being intercepted. The recorded stack even
pointed at `NestedUrlConnectionTests.java:78`, and nobody checked which line
that was.

Line 78 is not the assertion. It is the **stubbing** call:

```java
77:  NestedUrlConnection connection = mock(NestedUrlConnection.class);
78:  given(connection.getContentLength()).willCallRealMethod();   // <-- throws here
79:  given(connection.getContentLengthLong()).willReturn((long) Integer.MAX_VALUE + 1);
80:  assertThat(connection.getContentLength()).isEqualTo(-1);
```

`given(mock.m())` invokes `m()` on the mock. So the failing call is the FIRST
call on a freshly created mock, and it is not intercepted at all — no
self-call involved. Every later question the old page asked was aimed at the
wrong line.

## Root cause: an unpinned agent array carried across agent bytecode

`native_retransform_classes0` (`vm/src/runtime/instrument.rs`) walks the
`Class[]` an agent hands `Instrumentation.retransformClasses`. Mockito's inline
mock maker retransforms the target's whole superclass chain in ONE such call —
here `[java.net.URLConnection, java.lang.Object, NestedUrlConnection]`.

Each iteration runs the transformer chain (agent bytecode) and defines a
class. Both allocate, so both can safepoint and the collector can move things.
`arr` was a raw `ObjectRef` captured before the loop and never pinned. When the
array moved, the next `ctx.get_array_element(arr, i)` read the vacated address:

```rust
let mirror = match ctx.get_array_element(arr, i) {
    Value::Object(Some(o)) => o,
    _ => continue,          // <-- a zero word lands here
};
```

A zero word is not a crash. It is `Object(None)`, the `_ => continue` arm
swallows it, and **the remaining classes of the hierarchy are never
retransformed — silently, with nothing for the agent to see.** Mockito then
believes the mock is instrumented, hands it back, and the first call on it runs
the real body. `NestedUrlConnection.getContentLength()`'s real body calls
`getContentLengthLong()` → `connect()` → `this.resources` is null on an
Objenesis-built instance → NPE.

## The measurement that settled it

An instrumented build printed one line per class the loop actually applied.
A failing process against a passing one:

```
passing:  APPLY java/net/URLConnection      original=18368 final=29035
          APPLY java/lang/Object            original=2412  final=3971
          APPLY …/NestedUrlConnection       original=7567  final=10882

failing:  APPLY java/net/URLConnection      original=18368 final=29035
          (nothing else — i=1 and i=2 were silently dropped)
```

And after pinning, with a temporary line on the refresh path: **`ARRAY-MOVED
before i=1` fired in 60 of 60 processes.** The array moves every single time.
Whether the stale read still produced the right answer was luck — whether the
collector had reused the vacated bytes yet. That is the whole shape of the
~12% (bare probe) / ~2.4% (through the suite runner) rate, of its
load-sensitivity, and of its JIT-sensitivity (`--nojit` allocates differently
and was 0/170).

## Why every attempt to look at it from Java made it go away

Six in-process instruments were tried and every one of them repaired it: an
external call to the stubbed method before the self-call (0/60), a
`c.getClass()`, a `NestedUrlConnection.class` literal, registering a
`ClassFileTransformer`, calling `mock(Object.class)` first, and even setting
`CRATONVM_DBG_RETRANSFORM=1` (0/40). All of them change the allocation pattern,
and the allocation pattern is what decides whether the vacated array bytes have
been reused by the time the loop reads them.

That is why the page could measure the rate but never explain it: the only
usable instrument was on the VM side of the boundary, in the loop itself.

## The fix

Pin, and re-read from the pin before every element access — the idiom this
codebase already documents on `pin_native_root` and uses in the interpreter's
`checkcast`:

* `native_retransform_classes0`: the `Class[]`, the `Instrumentation`
  receiver, and the per-iteration class mirror (which is carried across
  `original_class_bytes` and a restore-to-original `retransform_class` before
  it reaches the transformer chain).
* `native_redefine_classes0`: the same, for the `ClassDefinition[]` loop —
  identical shape, identical hazard, not yet observed failing only because
  agents that call `redefineClasses` with several definitions are rarer.

The two arms that used to swallow an unreadable element now print a line
naming the class that was NOT retransformed. Silence is what made this cost a
day; a should-never-happen path that stays silent is a bug report nobody
files.

## Evidence

Binary: release build of dev `8d83c7585` plus this fix, Azure host 2.

| arm | before | after |
|---|---|---|
| minimal reproducer, processes | ~12% (7/60, 10/60, 5/40) | **0/140** |
| `NestedUrlConnectionTests` under generated load | ~2.4% (~9/370) | **0/100** |
| `NestedUrlConnectionTests`, suite runner, JIT on and off | — | PASS |
| the other five classes of the two retired pages | — | PASS, both modes |
| regression suite | — | 91/91 |

`cargo test -p cratonvm-vm -p cratonvm-jit -p cratonvm-types`: 7503 passed, 0
failed.

## Regression coverage, and why it is a source witness

Reproducing this needs a real agent, a real moving collector and luck, and
every in-process instrument that tried to observe it hid it — so a behavioural
test would be a coin flip that passes for the wrong reason. Instead two tests
in `vm/src/runtime/instrument.rs` assert the SHAPE that makes it impossible:
the array is pinned, it is re-read from that pin BEFORE `get_array_element`,
and the mirror is pinned, refreshed, and only then handed to the chain. Both
fail if the refresh is removed — checked by removing it.

## The same discipline one level down (2026-09-06, follow-up)

Auditing the twin the fix above names -- `redefineClasses0` -- turned up
nothing left in the loop itself: it was pinned in the same commit. What the
audit DID turn up is one level down, in `run_chain_over_bytes`, which both
loops funnel through.

That walk iterates `snapshot_transformer_chain()`, a `chain.to_vec()` copy.
The canonical chain is a GC root and is rewritten after a moving collection by
`remap_transformer_refs` -- whose own comment names the failure it exists to
stop: a `transformer_ref` left "pointing at the object's *old* address after
compaction ... a dispatch onto a relocated object the next time
`run_transformer_chain` invokes `transform`". The remap does not reach a
snapshot. So with two or more registered transformers, every entry after the
first is held across the previous transformer's `transform` call -- Java, which
allocates -- with no pin.

**Hardened, not demonstrated.** A probe that puts an allocating transformer
ahead of Mockito's, churns ~6 GB through it and calls `System.gc()` twice
inside the `transform` call was clean 20/20 on a pristine-dev control as well
as on the fix. The transformer object is registered once and long-lived, so
reaching it needs an old-gen compaction inside a transform call, and that probe
never produced one. The snapshot's staleness itself IS asserted
(`a_snapshot_taken_before_a_relocation_keeps_the_old_address`): the canonical
chain follows a relocation and the copy does not.

So this half is the same reference discipline applied where the shape exists,
at the cost of one pin per registered transformer -- not a crash anyone has
seen. Recorded that way on purpose: the array hazard above was measured at
~12% of processes, and running the two together under one "fixed" banner would
overstate the second.

The audit also closed the twin's remaining silence: `redefineClasses0`'s three
`continue` arms dropped a class the agent asked to redefine without a word,
which is precisely how the array bug stayed invisible for a day. They now name
the class they skipped.

## Repro (for a future regression)

The minimal reproducer is one mock and one call, in Spring Boot's own package
so the package-private class is visible:

```java
NestedUrlConnection c = mock(NestedUrlConnection.class);
when(c.getContentLength()).thenCallRealMethod();     // <-- throws here when it fires
when(c.getContentLengthLong()).thenReturn((long) Integer.MAX_VALUE + 1L);
```

Run it as ~60 separate PROCESSES, each doing a handful of iterations, while a
`cargo build --release` runs on the same host, and count processes that throw.
A process either fails on every iteration or none: the hierarchy is
half-instrumented for the life of the process. One long-running process proves
nothing.
