# ✅ FIXED — `SoftReference.get()` answered another Reference: the referent restore had no identity guard

## Status

**RESOLVED 2026-08-23** on `fix/bcjava-pqc-and-cipherstream-20260822`.

Filed the same day, from running `pqc.jcajce.provider.test.AllTests` to
completion for the first time. The page named it a default-collector defect and
gave a repro; this is the cause and the fix.

| | before | after |
|---|---|---|
| four bc-java `pqc` classes, 61 tests | `Failures: 0, Errors: 6` | **`OK (61 tests)`** |
| `ClassCache$CacheRef` cast lines | 21 | **0** |
| `pqc.jcajce.provider.test.AllTests` | red on some runs, `Errors: 24` on one | **`OK (316 tests)`**, zero occurrences |

## The defect

`process_references_after_gc` nulls the `referent` slot of every active
weak/phantom reference **before** the mark, so the mark cannot keep a referent
alive through its own (live) `Reference` — referent-slot hiding — and writes the
survivors back afterwards. Since 2026-08-15 the policy-condemned SOFT entries
ride the same pass.

Every guard on that write-back is about the **Reference**:

```rust
if shared.mem.heap.num_fields(ro) < 2 || !is_reference_shaped(ro) { continue }
if !identity_matches(ref_obj_old, ro) { continue }
shared.mem.heap.set_field(ro, 0, Value::Object(Some(rt)));   // <- rt had none
```

`rt` is resolved from a **pre-collection address**, and an address is not an
identity once a compacting collector has re-issued it. Survivors slide DOWN into
the space dead objects vacated, so a dead referent's base is very often a live
object's new base — the same collision the monitor table's
`prune_dead`-before-`remap_after_gc` ordering exists for, one table over.

The fallback that let it through is `watched_pre_gc_addr_survived`, whose ZGC
arm is `is_object_address(addr).is_some()`. That is a true statement — *an*
object lives there — and the wrong question. Its own doc comment calls the
G1/ZGC arms "already exact (live-region membership / registry lookup)", which is
exact about occupancy and silent about identity.

So the restore installed a stranger as somebody's referent, and the first
consumer to cast the result reported it:

```text
java.lang.ClassCastException: class java.io.ClassCache$CacheRef cannot be cast
to class java.lang.invoke.LambdaForm
    at java.lang.invoke.MethodTypeForm.cachedLambdaForm(MethodTypeForm.java:130)
```

with an `ObjectStreamClass.lookup` twin from the same run. Both victims are soft
caches that Java serialization warms up first, which is why the failures cluster
on `testPrivateKeyRecovery` / `testPublicKeyRecovery` / `testKeyPairEncoding`.

### Why only the default collector

* **Generational** emits an identity `pointer_map` entry for every *watched*
  address that survived without moving, so map membership is complete proof.
* **G1** does its reference processing in the remark pause
  (`g1_remark_process_references`), which has no pointer map and a
  dead-by-mark guard instead.
* **ZGC** relocates and hands back a map of MOVED objects only, so an in-place
  survivor is legitimately absent from it — and so is a dead object whose base
  was re-issued. The two are indistinguishable by address alone.

## The fix

Two screens on the referent, in increasing cost, both in the restore loop:

1. **A relocation TARGET is somebody else's address.** Every value in this
   cycle's `pointer_map` is a base a survivor moved *into*. If the referent's
   pre-collection address appears there — and the map is not itself what sent us
   there — the object that used to live at it is gone.
2. **Otherwise, the class must still match.** The pre-GC null pass reads slot 0
   on its way past, which is the one point in a collection that holds a referent
   and the heap at the same time, and records the referent's class id against
   the same `reference_obj` key the identity stamps use
   (`ReferenceProcessor::referent_class_stamps`). A class id is a header READ —
   unlike an identity hash it mints nothing, which matters when the collector is
   about to use that mark word.

A refusal leaves the slot null, which reads as a **cleared** reference: a legal
answer for a soft reference and an early one for a weak reference. Installing a
stranger is neither.

## The A/B, in one binary

`CRATONVM_GC=-referent-identity-screen` is a measurement-only escape hatch that
restores the pre-fix write. Four `pqc` classes (`XMSSTest`, `XMSSMTTest`,
`SLHDSATest`, `FalconTest`), 61 tests, same binary, same host, within one hour:

| arm | screen | result | `ClassCache$CacheRef` lines | refusals | wall |
|---|---|---|---|---|---|
| 1 | **ON** (shipping) | **`OK (61 tests)`** | **0** | 53 | 1344 s |
| 2 | OFF | `Failures: 0, Errors: 6` | 21 | 0 | 1432 s |
| 3 | **ON** | **`OK (61 tests)`** | **0** | 0 | 1761 s |
| 4 | OFF | `Failures: 0, Errors: 6` | 21 | 0 | 1279 s |

Both `OFF` arms fail identically — same count, same six tests — and both `ON`
arms are green. (`refusals` is only counted inside the screens, so an `OFF` arm
cannot report one. Arm 3 refused nothing and was green anyway: whether the
window opens at all is a property of the run, which is exactly why the page this
retires had to say the same thing about its failure counts.)

And the refusals name the mechanism rather than merely counting it:

```text
[refproc] REFUSE weak/phantom RESTORE ref @0x20042809be0:
          referent @0x20042808660 is a relocation TARGET this cycle (total refused=1)
[refproc] REFUSE weak/phantom RESTORE ref @0x20042809be0:
          referent @0x20042808660 is class 6, recorded 448 (total refused=2)
```

One reference, one referent address: caught by the first screen on the cycle the
slide happened, and by the second on the cycles after it. Both are load-bearing.

The screen is a hash lookup per active weak/soft entry per cycle, and the two
arms' wall clocks differ by less than the host's own noise.

## Regression

Same binary, everything the two retired bc-java pages own, plus the crate tests:

| | result |
|---|---|
| `pqc.jcajce.provider.test.AllTests` (the class this defect made red) | **`OK (316 tests)`**, 6178 s, zero `ClassCache` lines |
| `pqc.crypto.test.AllTests` | **`OK (120 tests)`**, 3217 s |
| `jce.provider.test.AllTests` | rc=0, 791 s |
| `cargo test -p cratonvm-gc -p cratonvm-vm --lib` | 2605 passed, **0 failed** |
| `cargo test -p cratonvm-jit --lib` | 2104 passed, 0 failed |
| `cargo test -p cratonvm-native-builtins --lib` | 4160 passed, 0 failed |

`046` had never been green on the default collector before this.

## What is not claimed

**Not that every stale-address write in reference processing is now guarded.**
This fixes the referent side of the restore pass, which is the one that writes an
OBJECT. The cleared/enqueue loops write a null and a queue link and already carry
the Reference-side identity stamp; `still_a_reference` screens the ref side only.
Nothing here re-audits them.

**Not that a same-class re-issue is impossible.** Screen 1 catches every address
a survivor slid into, which is the mechanism measured here. A referent that died
and whose base was handed to a *freshly allocated* object of the same class
would pass both screens. Closing that needs an identity the referent carries,
and minting one costs a mark-word write on every referent — priced and declined,
not overlooked.

**Not a bisect.** How long this had been live is unmeasured; the soft-reference
half of the pass is dated 2026-08-15 in its own comment, and the compaction it
depends on went default-on 2026-08-13.

## The transferable part

**A guard that proves the receiver says nothing about the value.** Three guards
stood over `set_field(ro, 0, rt)` and all three were about `ro`. The line writes
`rt`. Reading it aloud is enough — the asymmetry is visible in the call itself,
and it survived several rounds of hardening on the same function because each
round asked "is this still the Reference?" and never "is that still the
referent?".

**"Is there an object here" and "is this my object" are different questions, and
a relocating collector makes them different answers.** `is_object_address` is a
correct, exact predicate; it just does not answer what the caller needed. The
pointer map's VALUES are the missing half — they say which addresses changed
owner — and nothing was consulting them.
