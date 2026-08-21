# H0-8 — the "third mechanism" was four order artifacts of one yield, and my own probe was the confound

**Status: MEASURED.** Lane H0 (orchestrator), 2026-08-21, on
`C:/craton/cratonvm-r8.exe` at `025780ff7`. Oracle HotSpot 25.0.3+9. No source
change.

**Credit where it is due.** Lane `H17` was killed by a network fault (`ENOTFOUND`)
and got exactly one message out before it died:

> *"my ChmM probe was fully correct — and its M=1 case ran **first**. That
> suggests the inherited probe is **order-confounded**: cases 2–4 may be correct
> merely because they are not first."*

It was right, about a probe **I wrote and published results from**. This record
is the test it did not live to run.

---

## 1. What was claimed

`H13-1` reported a **third mechanism** distinct from `H0-5`'s A (a view carrier
with a null `this$0`) and B (a fabricated node class): under an armed
`java/util/concurrent/ConcurrentHashMap`, a map becomes *internally
inconsistent*. I reproduced it independently, added an "INDEPENDENT
REPRODUCTION" section to that record, and reported it twice as a live finding
with four discriminators:

1. four back-to-back `put`s give `size()=1` with an **empty** `keySet()`;
2. **interposing `size()` between the puts fixes it**;
3. **interposing `Math.abs(1)` — which does nothing to the map — also fixes it**;
4. `Integer` keys are correct where `String` keys are not;
5. the same map answers differently through a `ConcurrentHashMap`-typed local
   than through a `Map`-typed one.

Discriminators 2–5 were the interesting part. **All four are artifacts of the
order the cases ran in.**

## 2. The test — swap the order, watch the failure follow the position

Each probe runs two cases in one process, then again with the two swapped.

### 2a. The `Math.abs(1)` interposition

```
plain first     [1st] back-to-back      size=1  keys=[]
                [2nd] with Math.abs(1)  size=4  keys=[k0,k1,k2,k3]

abs first       [1st] with Math.abs(1)  size=1  keys=[]          <-- now IT fails
                [2nd] back-to-back      size=4  keys=[k0..k3]    <-- now IT passes
```

**`Math.abs(1)` has nothing to do with it.** The failure follows *position*, not
treatment.

### 2b. `String` versus `Integer` keys

```
String first    [1st] String keys  size=1
                [2nd] Integer keys size=4

Integer first   [1st] Integer keys size=1      <-- Integer keys break too
                [2nd] String keys  size=4      <-- String keys are fine
```

**Key type is irrelevant.** The `String`/`Integer` split — which I called "the
sharpest narrowing available" and handed to a lane as a lead — was position.

### 2c. The two doors

```
CHM read first  [1st] via CHM  containsKey=true  keySet=[only]
                [2nd] via Map  containsKey=false keySet=[]

Map read first  [1st] via Map  containsKey=true  keySet=[only]
                [2nd] via CHM  containsKey=false keySet=[]
```

**Not a door disagreement.** One map, two reads: whichever runs **first** is
right.

## 3. What is actually there

One phenomenon, not three mechanisms: **exactly one thing in the process
behaves differently, and which one is decided by order.** That is
`H16-3`'s finding — `CRATONVM_ENFORCE_NATIVE_SHADOW` **yields to real bytecode
exactly once per process** — seen from the `ConcurrentHashMap` side.

The single yield lands on whatever runs first; that one map (or that one read)
executes real JDK bytecode over state the VM owns, and fails. Everything after
takes the native path, which is self-consistent and correct-looking.

**So `H13-1`'s "third mechanism" is not a third mechanism.** It is the
instrument. The blast-radius dial does not simulate a retirement; it simulates
one retirement, once (`H0-4` §9).

### One asymmetry I am not going to paper over

The **puts** break on the FIRST map and succeed after. The **reads** succeed on
the FIRST read and fail after. Both are "exactly one differs", which is
consistent with a single yield, but **the polarity is opposite and I do not know
why.** Stating it because the tidy story would be "the first thing always
breaks", and that is not what the runs say.

## 4. What survives, and it is not nothing

* **The primary symptom is real.** `size()=1` with an empty `keySet()` on an
  armed CHM is an observed divergence from HotSpot, and the traced consequences
  in `H13-1` §4 stand: `CryptoPermissions.isEmpty()` true for a map whose
  `size()` is 1 → `JceSecurity.<clinit>` throws → the JCE is dead for the
  process. A failure observed under the dial is still a failure observed.
* **`H0-5` mechanisms A and B are untouched** — those were measured from stack
  traces and class names, not from a case-ordering.
* **The unarmed control was correct in every one of these runs**, so nothing
  here says the shipping default is broken by this.

## 5. How I got it wrong, since that is the reusable part

The probe put its cases in the order that told the story I already believed.
Case 1 was the symptom; cases 2 and 3 were "controls" that differed from case 1
in exactly the way I wanted to test — **and also in position.** With a
once-per-process effect, position is a hidden treatment applied to every case,
and I varied it in lockstep with the thing I was measuring.

The standing note in this directory is *a narrow probe reports its own reach,
not the defect*. This is the neighbouring failure and it needs its own name:
**a probe whose cases run in sequence inside one process is a repeated-measures
design, and anything latched per process is confounded with case order.** The
fix is trivial once seen — **run each case in its own process, or randomise and
repeat the order** — and neither is expensive.

I also handed these four "facts" to a lane as its starting brief. It found the
confound in the brief rather than in the VM.

## 6. NOMINATIONS

* **N1 — re-run every armed measurement in this directory as one case per
  process.** `H0-4`'s table, `H0-3`'s eleven, `H14-3`'s thirteen arms, `H22`'s
  screen. Each is a corpus run, so each vector is already its own process — **the
  vector-level numbers are probably safe**; it is the multi-case PROBES that are
  suspect. That distinction should be checked, not assumed.
* **N2 — find the latch.** `H16-3` nominates a process-global; this directory
  has a standing note that *a process-global `OnceLock` latches a guess
  forever*. Two independent symptoms now point at one.
* **N3 — the polarity asymmetry in §3.** Puts fail first, reads succeed first.
  Whatever explains that probably names the latch.
* **N4 — `regression-suite/probes/ChmConsistencyProbe.java` is confounded as
  filed.** Either split it per process or annotate it. Leaving it as-is invites
  the next reader to re-derive my error.
