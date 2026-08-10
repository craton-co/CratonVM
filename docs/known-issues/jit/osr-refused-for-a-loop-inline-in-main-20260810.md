# A hot loop inline in `main` never leaves the interpreter — OSR entry refused by a deopt point the entry cannot reach

**Status: OPEN, reproduced and diagnosed 2026-08-10, not fixed.** The identical
loop runs at **1 ns/iter in any called method and 180 ns/iter inline in `main`**
— 180x — because OSR entry is refused with a named reason. HotSpot runs both at
~0 ns/iter.

Found while auditing a *measurement*, not a workload: two probes in this repo's
WebFlux investigation put their benchmark loops inline in `main` and were
therefore measuring the interpreter, which produced a set of per-accessor ratios
that were wrong by ~10x. That is this defect's most likely real-world cost —
see [Who this actually hurts](#who-this-actually-hurts).

## Reproduction

`OsrProbe.java` (40-line probe, no framework). One loop, two placements,
selected by argv so each run contains exactly one:

```java
static long theLoop(int n) { long a=0; for (int i=0;i<n;i++) a += (i&7)+3; return a; }

// "method": the loop in a method, called twice
sink += theLoop(N); long t = System.nanoTime(); sink += theLoop(N);

// "main": the identical loop inline in main, which runs once
long acc=0; long t=System.nanoTime(); for (int i=0;i<N;i++) acc += (i&7)+3;
```

N = 40,000,000:

| | ns/iter | ms | `osr_entered` | `osr_refused_entry` |
|---|---:|---:|---:|---:|
| CratonVM, loop in a called method | **1** | 41 | 2 | 0 |
| CratonVM, loop inline in `main` | **180** | **7,232** | **0** | **5** |
| HotSpot, either | 0 | 16-19 | — | — |

`main` is called once, so OSR is the *only* route out of the interpreter for its
loop. It is refused, five times, and then the bounded per-pc rejection schedule
stops asking. 40M iterations run interpreted.

## The refusal, in the VM's own words

`CRATONVM_DBG=jitc`:

```
OSR-compile OsrProbe.main([Ljava/lang/String;)V entry_pc=76 entry=0x... len=13950
OSR-reuse   OsrProbe.main([Ljava/lang/String;)V entry_pc=76
OSR-refuse  OsrProbe.main([Ljava/lang/String;)V entry_pc=76
    JIT bailout [unsupported_shape]: osr-entry-unresumable-exit
    (deopt point at bci 28 (ReceiverTypeChanged) reconstructs an unresumable
     frame: stack 1 (Unsupported) of 2) (memoed)
```

The artifact compiles fine. Entry at the loop header (`entry_pc=76`) is then
refused because of a deopt point at **bci 28**, and `javap -c` says what bci 28
is:

```
23: getstatic     #17   // Field sink:J        <- pushes a long
26: ldc           #23   // int 40000000
28: invokestatic  #24   // Method theLoop:(I)J
31: ladd
```

That is the **other branch** — the `"method"` arm, which this run never
executes. At that call the operand stack holds a `long` (`sink`), and in a
method that uses long/float/double a non-oop stack entry is recorded
`FrameValue::Unsupported` rather than risk a truncated long on resume. So the
point is unresumable — and it disqualifies OSR entry at a pc in a different
branch that cannot reach it.

## Mechanism

`CompiledMethod::osr_exit_policy` (`jit/src/lib.rs`) opens with:

> *Classify what a mid-loop bail out of this artifact may do, refusing the entry
> outright when some **reachable** exit could not be resumed.*

and then iterates `for p in &self.deopt_points` — **every** point in the
artifact, with no reachability test against the OSR entry pc. The function has
only `&self`; computing reachability needs the method's bytecode, which the
`jit` crate does not have at that point (the same limitation is stated a few
lines below, for computing a successor bci).

This is a known defect *class* in-tree. `jit/src/x64/bytecode_walk.rs` already
says, about a different producer of the same veto:

> *`CompiledMethod::osr_exit_policy` is an artifact-wide veto: ONE unresumable
> deopt point refuses OSR entry at EVERY pc of the method … every counted loop
> in every method with a `long` accumulator was refused
> `osr-entry-unresumable-exit` … A once-invoked method whose loop is hot then
> never left the interpreter: ~90 ns/op against ~1.6 compiled.*

That was narrowed for OSR-exit maps (by restricting which pcs emit them). The
veto itself was left in place, and guard deopt points such as
`ReceiverTypeChanged` still trip it.

## What is NOT the trigger

Four attempted minimal reproducers all get OSR and run at 1 ns/iter, so do not
assume a wider blast radius than the evidence supports:

- a once-called *method* containing the loop (`OsrProbe2` bare/call/arg);
- a virtual call before the loop, same method;
- `String.equals` before the loop, same method;
- a ternary + `equals` prologue, same method (`OsrProbe3`);
- a dead `if` branch containing a call with a `long` on the operand stack,
  same method (`OsrVeto`) — the closest hand-built imitation of bci 28, and it
  still gets OSR.

Every one of those puts the loop in a method other than `main`. The reproducing
shape has the loop **inline in `main`**. Whether `main` is special because of
how the launcher invokes it, because of its `String[]` parameter, or because of
some combination not isolated here, is the open question — and it is the first
thing to settle, because it decides whether the fix is "implement the
reachability the doc already promises" or something narrower.

## Who this actually hurts

Not, on the evidence so far, application throughput: Spring's hot code is in
called methods, and the WebFlux investigation measured the whole OSR question as
worth ~nothing there (`osr_entered=0` on that workload, with the run's cost
diffuse — see
`springboot/webfluxautoconfigurationtests-recurring-timeout-hang-20260807.md`).

It hurts **measurement**. A benchmark whose loop sits in `main` — the default
way anyone writes a quick probe — measures the interpreter at ~180x. In this
repo it already has: `FloorProbe` shows 8/34/105 ns/iter for loops in a called
method against 131/314/745 for the identical loops inline in `main`, and the
first version of `ReflProbe` sat every reflection row on that ~700 ns floor and
produced ratios wrong by ~10x. HotSpot shows no difference between the two
shapes, so the trap is invisible if a probe is only sanity-checked against it.

Until this is fixed, **every arm of a CratonVM probe must live in its own small
method called many times**, and the control must use the same shape.

## Diagnostics added with this page

`osr_published_but_unenterable` and `osr_method_denied` now appear in the
`CRATONVM_DBG=jit-method-stats` OSR lifecycle line. The background-compile arm
that skips OSR — when a published artifact reports no enterable offset for the
pc, or the method is OSR-denied — returned without recording anything, so that
line could read `osr_entered=0 osr_refused_entry=0` next to a non-zero `osr=`
compile count, which reads as "OSR was never tried".

**Both counters were zero in every run measured here, including the WebFlux one
that motivated them.** They close a hole that demonstrably exists in the code
(the arm returns `Skip` with no metric) but they have not been observed firing,
so do not read a future zero as evidence of anything until one of them has been
seen non-zero at least once.

That also corrects a guess made while writing this page: WebFlux's
`osr_entered=0` is **not** the un-enterable-artifact arm. With all three
counters at zero and one OSR compile recorded, what happened there is that the
compile was requested, produced, and the back edge never came round often enough
to enter it — i.e. that workload has no loop hot enough to matter, which is
consistent with everything else measured about it.
