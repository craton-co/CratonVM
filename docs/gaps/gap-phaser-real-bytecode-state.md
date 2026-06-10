# Gap: real `java.util.concurrent.Phaser` bytecode misexecutes (state reads 0, `root` reads null)

## Status
OPEN — discovered 2026-06-10 during the j.u.c. descriptor-coercion audit
(Semaphore/CyclicBarrier/Exchanger/Phaser holder fixes). Pre-existing: reproduces
on unmodified dev (`target/release/cratonvm.exe` from merge `d559e21a`).

## Symptom
In real-JDK builds the synthetic Phaser natives are NOT registered
(`register_phaser_natives` is only reachable from `register_synthetic_overrides`,
which is `cfg(feature = "synthetic-jdk")`), so the REAL Phaser bytecode runs — and
misbehaves:

```java
Phaser ph = new Phaser(2);
ph.getRegisteredParties();   // returns 0 (HotSpot: 2)
ph.getPhase();               // returns 0 (correct, but also the default)
ph.getArrivedParties();      // returns 0 (correct value, but suspicious given below)
ph.getUnarrivedParties();    // THROWS:
// java/lang/NullPointerException: Cannot read field 'state' because the object is null
//   at java/util/concurrent/Phaser.reconcileState(Phaser.java:493)
//   at java/util/concurrent/Phaser.getUnarrivedParties(Phaser.java:866)
```

Repro: `PhMini.java` / the Phaser section of `JucProbe.java` (probe sources in
`%TEMP%\jucprobe`; trivially recreatable from the snippet above).

## Analysis so far
- `new Phaser(2)` → `this(null, 2)` → the `(Phaser,int)` ctor must set
  `this.root = this` and `state = (parties<<16)|parties = 0x20002`. Under
  CratonVM `getRegisteredParties()` (= `partiesOf(reconcileState())`) returns 0
  and `reconcileState()`'s `root.state` read NPEs — i.e. BOTH the `state` long
  (slot 0, J) and `root` (slot 2, L) fields read back as 0/null after the ctor.
- Two candidate mechanisms, not yet discriminated:
  1. the ctor's `putfield`s land in the wrong slots / are dropped (constructor
     delegation or final-field write path), or
  2. the reads resolve to the wrong slots (e.g. a stale layout for Phaser from a
     partially-synthetic class load).
- Inconsistency worth chasing: `getArrivedParties()` (also `reconcileState()`-based)
  returned 0 WITHOUT throwing, while `getUnarrivedParties()` threw — so the
  `root == this ? state : root.state` branch behaves differently between two
  adjacent calls on the same receiver. Suggests an unstable field read, not a
  simple "ctor never wrote" story.

## Workaround options
- Register the (now coercion-safe, int[3]-holder-based) synthetic Phaser natives
  in real-JDK builds too — they pass the synthetic-side semantics and would mask
  this. NOT done: it's the shadow-real-bytecode pattern the stub-removal effort
  is eliminating; fix the underlying interpreter issue instead.

## Affected
Anything using `Phaser` on the real-JDK build (e.g. `Phaser`-based test
coordinators, fork-join style barriers in app code).
