# H0-8 — the "third mechanism" was four order artifacts of one yield, and my own probe was the confound

> **CORRECTION 2026-08-22 — the counting claim in this record is
> withdrawn (`H17-2` N4).** The `table` array class is not a yield
> counter. It reports **one bit per map** — whether that map's FIRST
> insert ran bytecode — so it cannot distinguish "this method never
> yielded" from "this method yielded later, after the native had already
> allocated the table". Every observation here stands; every statement of
> the FORM "the dial yields once per X" does not.
>
> The mechanism is now settled and it was never a yield budget: the dial
> had one live call site of fourteen dispatch doors, so an armed class
> yielded only on the dispatches that reached step 1 cold. Fixed
> 2026-08-21 — all fourteen doors consult it, and an armed class now
> yields on every covered dispatch that has bytecode to yield to. See
> `WORKER-1-the-dial-now-reaches-every-door-20260821.md`.

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

---

## 7. CORRECTION (lane H17, 2026-08-21) — the mechanism in §3 is wrong, and §3's open puzzle is now answered

§3 said the effect is a yield "**exactly once per process**". **That is not the
mechanism**, and lane `H17` — which also retracted its own first record mid-lane
for the same error — found the real one.

### The witness reports one BIT, and three of us read it as a COUNT

The `HashMap.table` array class answers *"did this map's FIRST insert run real
bytecode?"* — **one bit per map, not a yield counter.** `H16-3` read it as a
count, I repeated that reading in §3, and `H17-1` §2 did too before catching
itself. The census from a single armed run shows **four distinct triples on
`HashMap` each winning bytecode**, which a once-per-process model cannot produce.

**So "exactly one differs" was an artifact of a one-bit witness**, on top of the
case-ordering artifact this record already documents. Two layers of instrument
error stacked, and the second was invisible until the first was removed.

### What is actually true — VERIFIED independently

```
$ git grep -n "jdk_only_enforce_shadow_for" -- '*.rs'
vm/src/runtime/env_cache.rs:752:            pub fn jdk_only_enforce_shadow_for(...)   <- definition
vm/src/runtime/interpreter/native_override.rs:2499:  /// … doc comment
vm/src/runtime/interpreter/native_override.rs:7433:  strict_bridge && … enforce_shadow_for(class_name);
```

**One definition, one doc mention, exactly ONE live call site** — inside
`resolve_step1_native`. So:

> **Arming a class does not arm the class. It arms that class's COLD, step-1
> dispatches.** Warm invoke-cache entries, the force-native interceptor,
> reflective `Method.invoke` and JIT binds are all outside the dial's reach *by
> construction*.

### This answers the polarity asymmetry §3 left open

§3 recorded, and declined to explain, that **puts break on the first map while
reads succeed on the first read**. Under the correct mechanism there is no
asymmetry to explain: **the FIRST use of a call site is a cold dispatch and is
dialled; later uses hit the warm cache and are not.** Whether being dialled
helps or hurts depends on the operation — a put through real bytecode over
VM-owned state fails, a read through it may succeed. Same mechanism, opposite
outcomes, no contradiction.

I flagged that asymmetry rather than smoothing it over, and it turned out to be
the thread that named the cause.

### Both of the fix hypotheses I put in H17's brief were wrong

I briefed the lane that the dedup was probably short-circuiting enforcement, and
pointed at `env_cache`'s `OnceLock`s. Measured:

* `ask = strict_bridge && (enforce || !already_observed)` — **`enforce ||`
  short-circuits, so the doc comment's stated intent holds exactly as written.**
  No one-line bug there.
* the `OnceLock`s latch only the **scope string** — static config, correct to
  latch.

The lane disproved its own brief before doing the work, which is the outcome a
brief should make possible.

### The instrument is in worse shape than "it yields once"

`H17` measured that **four of the six witnesses this directory has used are
blind on a current binary** — bucket head class (because `H16-2` taught the
native to mint real `HashMap$Node`s, so the witness now agrees in both
directions), `modCount`, `hashCode()` counts and `equals()` counts. Only the
`table` array class still discriminates, and only coarsely. The census is a
**deduplicated presence set with no counts**, and under `enforce` it records only
the bytecode-won half.

**A fix to the VM removed a witness.** That is worth carrying: as the VM gets
more correct, the instruments built to detect its incorrectness stop working,
silently.
