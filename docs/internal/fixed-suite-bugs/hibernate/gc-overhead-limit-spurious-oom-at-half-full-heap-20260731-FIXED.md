# FIXED — spurious `OutOfMemoryError` with 570 MB free: the young generation had lost its only drain

| | |
|---|---|
| **Status** | ✅ **FIXED** 2026-07-31 — `fix/hib-gcoverhead-halffull-20260731`. Real VM defect, root-caused and fixed at source; the mitigation the original report asked for was also added, as a safety net. |
| **ID** | `HIB-GCOVERHEAD-HALFFULL.1` |
| **Found** | 2026-07-31, validating the `DefaultCatalogAndSchemaTest` runner accommodation. |
| **Repro** | **`probes/GcPromoteProbe.java`** — deterministic, seconds, no Hibernate. Use this one, and note it still reproduces on the `dev` tip. |
| **Original repro** | `DefaultCatalogAndSchemaTest`, JIT on, `--Xmx 1500m`, real JDK — OOM at ~41 min, 3 for 3 against `32f9db9a2`. **No longer discriminating**: it now passes on the unfixed `dev` tip too. See Verification. |

## Symptom

```
Exception in thread "main" java/lang/OutOfMemoryError: Java heap space
    (anewarray component 6 length 644)
```

A 644-element reference array — roughly 5 KB — failed to allocate on a heap
that was **49 % full with ~570 MB free**, after thirty forced GCs that every
one of them reported `promoted=0`.

## What was actually wrong

The original report's proximate diagnosis was right and its *underlying* one
was not. It attributed the wedge to fragmentation from the non-moving sweep's
free-list allocation. The real cause is simpler and is a plain regression:

**The non-moving young sweep's SELECTIVE PROMOTION — the young generation's
only young→old drain while a JIT frame is live, and therefore the only one that
runs at all in a warmed-up process — had switched itself off VM-wide.**

That is why `promoted=0` on all thirty cycles, and why the old generation sat
nearly empty while young was packed with live objects that could never leave
it. Not fragmentation: young simply had no exit.

### How it switched off

`gen_heap.rs`'s `selective_on` was gated on
`gc_quiescence::moving_young_coverage_incomplete()`. When that gate was written
(`a35ed0aeb`, xt-hardening 2026-07-03) the flag had **exactly one caller** — the
cross-thread takeover path — and it meant:

> this cycle scanned state belonging to a peer that will never re-read its own
> registers, so a derived/interior pointer in that peer would be left dangling
> if we evacuated the base it points into

which is a real hazard and a correct reason to suppress promotion for a cycle.

`arch-2026-07-26` (`moving-young-precise-roots`) then reused the same flag for
an entirely different question — *"may the COPYING collector relocate this
cycle?"* — and gave it a dozen new callers. Once moving-young became the
default, the flag was set on **essentially every JIT-active collection**:
`compiled-frame-oop-not-published`, `missing-exact-rbp`,
`unregistered-jit-frame-on-stack`, `active-safepoint-map-incomplete`. The
promotion gate, reading a flag that had quietly changed meaning underneath it,
suppressed the drain on every one of those cycles.

Those coverage reasons are **not** the hazard the gate exists for. They describe
compiled frames that the conservative scan still walks, and selective promotion
is safe under exactly that regime: it pins by raw slot **value**, so a
conservatively-discovered address — real oop or false positive — is never
evacuated. Only a peer *excused from the STW barrier* (OS-suspended in JIT, or a
blocked peer's helper window) can hold state that neither pin-by-value nor the
post-GC remap protects.

### The differential that identified it

`CRATONVM_NO_MOVING_YOUNG=1` on the **unfixed** binary makes the whole failure
disappear — because `refresh_moving_young_coverage_for_collection()` returns
early when moving-young is off, so the flag is never set and promotion survives:

| arm (same binary, `probes/GcPromoteProbe.java`, `--Xmx 128m`, JIT on) | result |
|---|---|
| default | `promoted=0`, **permanently wedged** — no progress past 20 MB in 300 s |
| `CRATONVM_NO_MOVING_YOUNG=1` | `promoted=25 MB` on the first draining cycle, **completes in 3.9 s** |

## The fix

Two layers, both in this branch.

### 1. Split the two verdicts (the real fix)

`gc_quiescence::unrewritable_peer_state()` is a new, strictly narrower per-cycle
verdict, armed only by `XT_TAKEOVER` and `XT_HELPER_WINDOW` (plus the
takeover site in `interpreter.rs` that calls the bare marker). `selective_on`
reads that instead of the wide coverage verdict. `moving_young_coverage_incomplete`
keeps its own meaning and keeps diverting the copying collector exactly as
before — nothing about relocation policy changed.

`CROSS_THREAD_JIT_PEER` is deliberately **not** in the narrow set. It means only
"some peer is somewhere inside compiled code", which is true of nearly every
multi-threaded cycle in a server workload; such a peer is parked *cooperatively*
and its own deposited root snapshot (including its conservative JIT-frame scan)
is folded into the collection's roots, so pin-by-value already covers it.
Classifying it as un-rewritable would re-create this very bug for every
multi-threaded application.

### 2. The other half of `UseGCOverheadLimit` (the safety net)

`note_gc_productivity` implemented only the freed-bytes half of HotSpot's
`UseGCOverheadLimit`. HotSpot requires a GC-time fraction **and** a free-space
condition before it converts GC pressure into an `OutOfMemoryError`; CratonVM
checked only the first, so any defect that stops young draining read identically
to a genuine retained-allocation death spiral.

The added condition is the death spiral's own defining fact, taken verbatim from
the function's existing doc comment — *"a wedged, ~full old generation cannot
absorb 2 % of total heap capacity per cycle"*. A cycle now counts toward the
streak only when it freed < 2 % of capacity **and** the old generation cannot
absorb 2 % of capacity.

This is deliberately **not** a total-fullness gate. Those were rejected when the
limit was written, for a reason that still holds: in the real spiral the young
semi is emptied every cycle, so *total* fullness parks near young/total and never
looks exhausted. Old-generation headroom is the question that actually
discriminates. `CRATONVM_DBG_GC_OVERHEAD=1` now prints `old_headroom=`,
`freed_sliver=` and `old_gen_wedged=` so the two halves are separable in a log.

The original report's suggested predicate (ample headroom **and** `promoted=0`)
was equivalent here but would have gone inert the moment layer 1 landed, since
promotion is no longer zero. Old-gen headroom keeps working.

## Verification

**`probes/GcPromoteProbe.java` is the authoritative repro — not the Hibernate
class.** `--Xmx 128m`, JIT on, 36 MB live set; deterministic, and it answers in
seconds:

| binary | result |
|---|---|
| base `32f9db9a2` | `promoted=0`, `unproductive=true`, **wedged** — no progress past 20 MB |
| `origin/dev` @ `cc8167f94` (2026-08-01) | `promoted=0`, `unproductive=true`, **wedged**, times out at 240 s |
| **this branch** | `promoted` 2–4.6 MB *per cycle*, completes in **2.3 s** |
| HotSpot control | 0.14 s |

The second row is the one to keep: the defect is **still live on the current
`dev` tip**, which carries the `selective_on` gate unmodified. Re-check with this
probe, never with the Hibernate class — see below.

### `DefaultCatalogAndSchemaTest` — and why it is no longer the repro

On this branch the class runs **`132/132 failed=0`, 3 runs for 3**, matching the
HotSpot control exactly, and `CRATONVM_DBG_GC_OVERHEAD=1` prints **not one line**
across a whole run — *zero* forced GCs, where the failing run took thirty and
latched the streak on eight of them. Progress is linear; the signature collapse
(104 tests in the first 41 min, then 7 more in the next 44) is gone.

**But the class also passed 132/132 on the unfixed `dev` tip.** The original
report's "OOM at ~41 min, 3 for 3" was measured against `32f9db9a2`; `dev` has
since moved 187 files, and the class's allocation profile no longer reliably
crosses the threshold. It is *not* evidence that the defect is gone — the probe
shows it is not — and it is *not* evidence that this fix is unnecessary. It only
means this class stopped being a discriminator, which is precisely why the probe
was written and committed alongside the fix.

Wall time on the class is ~35–50 min against HotSpot's 120 s. That gap is the
[moving-young-inert-under-JIT](../../../known-issues/hibernate/moving-young-inert-under-jit-throughput-tax-20260730.md)
throughput tax and stays with it. (An earlier "105 min" figure recorded here was
measured with three full workspace builds running concurrently — discount it.)

### What that took: a second fix

The first clean run did not come for free. Restoring the drain also re-enabled
evacuation of roots published as **movable** precise-JIT roots — on cycles whose
coverage proof had just failed. Three runs with the drain restored but that
hazard unbounded produced three different outcomes (`found=121`; a
`AbstractMethodError` on an interface's abstract declaration; a SIGSEGV in a JIT
frame), all inside `[class-template-invocation:#12]`.

It is not a regression from this fix — running the same class with
`CRATONVM_NO_SELECTIVE_PROMOTE=1`, i.e. the collector's pre-fix behaviour, fails
invocation #12 *worse* (4 and 5 tests, twice). This fix made it reachable, not
real. Root cause, verification and the differential that found it:
[`invocation12-late-phase-instability-movable-jit-root-20260801-FIXED.md`](invocation12-late-phase-instability-movable-jit-root-20260801-FIXED.md).

### Regression tests

Both assert the **decision**, not the symptom. An end-to-end "does it still OOM"
test cannot distinguish this defect from any other allocation failure, and would
start passing again the moment something unrelated made the young semi big
enough to hide it.

- `gc_quiescence::tests::unrewritable_peer_state_is_narrower_than_the_coverage_verdict`
  — each of the eleven this-thread/cooperative reasons must still divert the
  copying collector and must **not** arm the promotion gate; the two
  cross-thread reasons must arm it; the verdict is per-cycle and is not
  first-wins.
- `gen_heap::tests::selective_promotion_runs_under_an_unproven_frame_map_but_not_a_frozen_peer`
  — at the collector: objects are promoted under `UNPUBLISHED_FRAME_OOP` and are
  not under `XT_TAKEOVER`. **Verified to fail against the old gate** before
  being accepted, so it is a genuine differential rather than a test that
  happens to pass.
- `gen_heap::tests::a_movable_jit_root_is_pinned_when_the_coverage_proof_failed`
  — the bound from the second fix, asserted in both directions.

## What this does NOT fix

The moving-young coverage gap itself is untouched and still open: four separate
obligations (`compiled-frame-oop-not-published`,
`unregistered-jit-frame-on-stack`, `missing-exact-rbp`,
`active-safepoint-map-incomplete`) each still divert the young collection to the
non-moving sweep, so the copying collector effectively never runs under the JIT.
That remains a **throughput** tax owned by
[`../../../known-issues/hibernate/moving-young-inert-under-jit-throughput-tax-20260730.md`](../../../known-issues/hibernate/moving-young-inert-under-jit-throughput-tax-20260730.md)
and its authority
[`../../jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`](../../jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md).

What has changed is that it is *only* a throughput tax again. The correctness
failure this report recorded — the process dying with `OutOfMemoryError` on a
half-empty heap — was never a consequence of running the non-moving sweep. It
was a consequence of running the non-moving sweep **with its drain disabled**,
which was a separate, independently-fixable regression.

## Lesson

A boolean whose meaning is documented at its single call site will be reused,
and the reuse will not update the reader. `moving_young_coverage_incomplete`
went from one caller to a dozen and from "un-rewritable peer state" to "cannot
relocate" without anything forcing the promotion gate to be re-read. The
symptom appeared in a completely different subsystem (the allocator's OOM path),
attached itself to a plausible-but-wrong story (fragmentation), and was only
separable by an A/B on a flag — `CRATONVM_NO_MOVING_YOUNG` — that has no
apparent connection to promotion at all.
